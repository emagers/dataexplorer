use crate::model::{Table, numeric_type, text};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChartKind {
    Line,
    Scatter,
    Time,
    Bar,
}
#[derive(Clone, Debug)]
pub struct Series {
    pub name: String,
    pub points: Vec<(f64, f64)>,
}
#[derive(Clone, Debug)]
pub struct ChartData {
    pub kind: ChartKind,
    pub title: String,
    pub x_title: String,
    pub y_title: String,
    pub series: Vec<Series>,
    pub x_bounds: [f64; 2],
    pub y_bounds: [f64; 2],
    pub categories: Vec<String>,
}
fn prop<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
        .filter(|v| !v.is_null())
}
fn names(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}
fn bounds(values: impl Iterator<Item = f64>) -> [f64; 2] {
    let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
    for n in values {
        min = min.min(n);
        max = max.max(n);
    }
    if min == max {
        [
            min - min.abs().max(1.) * 0.05,
            max + max.abs().max(1.) * 0.05,
        ]
    } else {
        [min, max]
    }
}
fn number(v: &Value) -> Result<f64> {
    let n: f64 = text(v).parse().context(
        "chart cell is not numeric; null/dynamic series require an explicit query projection",
    )?;
    ensure!(n.is_finite(), "chart contains a non-finite numeric value");
    Ok(n)
}
pub fn prepare(table: &Table, rows: &[usize]) -> Result<ChartData> {
    let metadata = table
        .visualization
        .as_ref()
        .context("no Kusto Visualization metadata; add a render operator")?;
    let visualization = prop(metadata, "Visualization")
        .and_then(Value::as_str)
        .context("Visualization type missing")?;
    let kind = match visualization.to_lowercase().as_str() {
        "linechart" => ChartKind::Line,
        "timechart" => ChartKind::Time,
        "scatterchart" => ChartKind::Scatter,
        "barchart" => ChartKind::Bar,
        other => bail!("unsupported visualization {other}; table retained"),
    };
    for key in ["XColumn", "YColumns", "Series"] {
        if let Some(v) = prop(metadata, key) {
            ensure!(
                v.is_string() || v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
                "invalid {key} visualization property"
            );
        }
    }
    for (key, allowed) in [
        ("Kind", "default"),
        ("Xaxis", "linear"),
        ("Yaxis", "linear"),
        ("Ysplit", "none"),
    ] {
        if let Some(v) = prop(metadata, key).and_then(Value::as_str) {
            ensure!(
                v.is_empty() || v.eq_ignore_ascii_case(allowed),
                "{key}={v} is unsupported; table retained"
            );
        }
    }
    ensure!(
        !prop(metadata, "Accumulate").is_some_and(|v| v == true || v == "true"),
        "accumulate is unsupported; table retained"
    );
    for key in ["Xmin", "Xmax", "Ymin", "Ymax"] {
        ensure!(
            prop(metadata, key)
                .is_none_or(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("nan"))),
            "custom {key} bound unsupported; table retained"
        );
    }
    ensure!(!rows.is_empty(), "no visible rows to chart");
    ensure!(
        rows.len() <= 10_000,
        "chart exceeds 10000 rows; filter locally or aggregate the query (exports are never sampled)"
    );
    let find = |name: &str| {
        table
            .columns
            .iter()
            .position(|c| c.name == name)
            .with_context(|| format!("chart column {name:?} not found"))
    };
    let x = match names(prop(metadata, "XColumn")).first() {
        Some(name) => find(name)?,
        None => {
            if kind == ChartKind::Time {
                table
                    .columns
                    .iter()
                    .position(|c| c.kind == "datetime")
                    .context("timechart requires a datetime column")?
            } else {
                0
            }
        }
    };
    let y_names = names(prop(metadata, "YColumns"));
    let y: Vec<usize> = if y_names.is_empty() {
        table
            .columns
            .iter()
            .enumerate()
            .filter(|(i, c)| *i != x && numeric_type(&c.kind))
            .map(|(i, _)| i)
            .collect()
    } else {
        y_names
            .iter()
            .map(|name| find(name))
            .collect::<Result<_>>()?
    };
    ensure!(!y.is_empty(), "chart requires a numeric y column");
    let grouping = names(prop(metadata, "Series"))
        .iter()
        .map(|name| find(name))
        .collect::<Result<Vec<_>>>()?;
    let mut series: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    let mut categories = Vec::new();
    for (ordinal, &i) in rows.iter().enumerate() {
        let row = table.rows.get(i).context("chart row out of range")?;
        ensure!(x < row.len(), "chart x column out of range");
        let xv = match kind {
            ChartKind::Time => {
                chrono::DateTime::parse_from_rfc3339(&text(&row[x]))
                    .context("timechart x must be RFC3339 datetime")?
                    .timestamp_millis() as f64
                    / 1000.
            }
            ChartKind::Bar => {
                categories.push(text(&row[x]));
                ordinal as f64
            }
            _ => number(&row[x])?,
        };
        let group = grouping
            .iter()
            .map(|i| text(&row[*i]))
            .collect::<Vec<_>>()
            .join(" / ");
        for &col in &y {
            ensure!(
                numeric_type(&table.columns[col].kind),
                "chart y column must be numeric"
            );
            let label = if group.is_empty() {
                table.columns[col].name.clone()
            } else {
                format!("{group}: {}", table.columns[col].name)
            };
            series
                .entry(label)
                .or_default()
                .push((xv, number(&row[col])?));
        }
    }
    ensure!(
        series.len() <= 32,
        "more than 32 series; chart refused without dropping series"
    );
    let series: Vec<_> = series
        .into_iter()
        .map(|(name, points)| Series { name, points })
        .collect();
    let mut x_bounds = bounds(series.iter().flat_map(|s| s.points.iter().map(|p| p.0)));
    let mut y_bounds = bounds(series.iter().flat_map(|s| s.points.iter().map(|p| p.1)));
    if kind == ChartKind::Bar {
        y_bounds[0] = y_bounds[0].min(0.);
        y_bounds[1] = y_bounds[1].max(0.);
        x_bounds = [-0.5, rows.len() as f64 - 0.5];
    }
    ensure!(
        x_bounds
            .iter()
            .chain(y_bounds.iter())
            .all(|v| v.is_finite())
            && x_bounds[0] < x_bounds[1]
            && y_bounds[0] < y_bounds[1],
        "chart bounds cannot be represented safely as f64; table retained"
    );
    Ok(ChartData {
        kind,
        title: prop(metadata, "Title")
            .and_then(Value::as_str)
            .unwrap_or(visualization)
            .into(),
        x_title: prop(metadata, "XTitle")
            .and_then(Value::as_str)
            .unwrap_or(&table.columns[x].name)
            .into(),
        y_title: prop(metadata, "YTitle")
            .and_then(Value::as_str)
            .unwrap_or("value")
            .into(),
        series,
        x_bounds,
        y_bounds,
        categories,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Column, Table};
    use serde_json::json;
    #[test]
    fn negative_fractional_bars_are_not_integer_cast() {
        let table = Table {
            id: 0,
            name: "t".into(),
            kind: "PrimaryResult".into(),
            columns: vec![
                Column {
                    name: "label".into(),
                    kind: "string".into(),
                },
                Column {
                    name: "y".into(),
                    kind: "real".into(),
                },
            ],
            rows: vec![vec![json!("a"), json!(-1.25)], vec![json!("b"), json!(0.5)]],
            visualization: Some(
                json!({"Visualization":"barchart","XColumn":"label","YColumns":["y"]}),
            ),
        };
        let chart = prepare(&table, &[0, 1]).unwrap();
        assert_eq!(chart.series[0].points, [(0., -1.25), (1., 0.5)]);
        assert_eq!(chart.y_bounds, [-1.25, 0.5]);
        let mut invalid = table.clone();
        invalid.visualization = Some(json!({"Visualization":"piechart"}));
        assert!(prepare(&invalid, &[0]).is_err());
        invalid.visualization = Some(json!({"Visualization":"barchart","Accumulate":true}));
        assert!(prepare(&invalid, &[0]).is_err());
        let mut defaults = table.clone();
        defaults.visualization.as_mut().unwrap()["Ymin"] = json!("NaN");
        defaults.visualization.as_mut().unwrap()["Ymax"] = json!("NaN");
        assert!(prepare(&defaults, &[0, 1]).is_ok());
    }
}
