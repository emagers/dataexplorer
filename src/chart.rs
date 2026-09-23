use crate::model::{Table, numeric_type, text};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChartColumns {
    pub x: usize,
    pub y: Vec<usize>,
    pub series: Vec<usize>,
}

pub fn x_compatible(kind: ChartKind, column_type: &str) -> bool {
    match kind {
        ChartKind::Time => matches!(column_type, "datetime" | "System.DateTime"),
        ChartKind::Line | ChartKind::Scatter => numeric_type(column_type),
        ChartKind::Bar => true,
    }
}

impl ChartColumns {
    pub fn infer(table: &Table, kind: ChartKind) -> Result<Self> {
        let x = table
            .columns
            .iter()
            .position(|c| x_compatible(kind, &c.kind))
            .context("no compatible X column (timechart requires datetime; line/scatter require numeric)")?;
        Ok(Self {
            x,
            y: table
                .columns
                .iter()
                .enumerate()
                .filter(|(i, c)| *i != x && numeric_type(&c.kind))
                .map(|(i, _)| i)
                .collect(),
            series: Vec::new(),
        })
    }

    pub fn validate(&self, table: &Table, kind: ChartKind) -> Result<()> {
        let x = table.columns.get(self.x).context("X column out of range")?;
        ensure!(
            x_compatible(kind, &x.kind),
            "X must be datetime for timechart or numeric for line/scatter"
        );
        ensure!(!self.y.is_empty(), "Select at least one numeric Y column");
        ensure!(self.y.len() <= 32, "Select at most 32 Y columns");
        let mut selected = BTreeSet::from([self.x]);
        for &index in self.y.iter().chain(&self.series) {
            ensure!(index < table.columns.len(), "chart column out of range");
            ensure!(
                selected.insert(index),
                "X, Y and series columns must be distinct"
            );
        }
        ensure!(
            self.y.iter().all(|&i| numeric_type(&table.columns[i].kind)),
            "Y columns must be numeric"
        );
        Ok(())
    }
}

pub fn kind(table: &Table) -> Result<ChartKind> {
    let metadata = table
        .visualization
        .as_ref()
        .context("no Kusto Visualization metadata; add a render operator")?;
    match prop(metadata, "Visualization")
        .and_then(Value::as_str)
        .context("Visualization type missing")?
        .to_lowercase()
        .as_str()
    {
        "linechart" => Ok(ChartKind::Line),
        "timechart" => Ok(ChartKind::Time),
        "scatterchart" => Ok(ChartKind::Scatter),
        "barchart" => Ok(ChartKind::Bar),
        other => bail!("unsupported visualization {other}; table retained"),
    }
}

pub fn default_columns(table: &Table) -> Result<ChartColumns> {
    let kind = kind(table)?;
    let metadata = table.visualization.as_ref().context("no visualization")?;
    for key in ["XColumn", "YColumns", "Series"] {
        if let Some(v) = prop(metadata, key) {
            ensure!(
                v.is_string() || v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
                "invalid {key} visualization property"
            );
        }
    }
    let find = |name: &str| {
        let mut matches = table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.name == name);
        let (index, _) = matches
            .next()
            .with_context(|| format!("chart column {name:?} not found"))?;
        ensure!(
            matches.next().is_none(),
            "ambiguous chart column {name:?}; choose columns with F3"
        );
        Ok(index)
    };
    let x_names = names(prop(metadata, "XColumn"));
    ensure!(x_names.len() <= 1, "select exactly one X column");
    let x = match x_names.first() {
        Some(name) => find(name)?,
        None => ChartColumns::infer(table, kind)?.x,
    };
    let series: Vec<usize> = names(prop(metadata, "Series"))
        .iter()
        .map(|name| find(name))
        .collect::<Result<_>>()?;
    let y_names = names(prop(metadata, "YColumns"));
    let y = if y_names.is_empty() {
        table
            .columns
            .iter()
            .enumerate()
            .filter(|(i, c)| *i != x && !series.contains(i) && numeric_type(&c.kind))
            .map(|(i, _)| i)
            .collect()
    } else {
        y_names
            .iter()
            .map(|name| find(name))
            .collect::<Result<_>>()?
    };
    let columns = ChartColumns { x, y, series };
    columns.validate(table, kind)?;
    Ok(columns)
}

pub fn time_label(seconds: f64, span: f64) -> String {
    chrono::DateTime::from_timestamp_millis((seconds * 1000.).round() as i64)
        .map(|t| {
            t.format(if span >= 2. * 86400. {
                "%Y-%m-%d"
            } else if span >= 120. {
                "%Y-%m-%d %H:%M"
            } else if span >= 1. {
                "%Y-%m-%d %H:%M:%S"
            } else {
                "%Y-%m-%d %H:%M:%S%.3f"
            })
            .to_string()
        })
        .unwrap_or_else(|| format!("{seconds:.3}"))
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
    prepare_with_columns(table, rows, None)
}

pub fn prepare_with_columns(
    table: &Table,
    rows: &[usize],
    columns: Option<&ChartColumns>,
) -> Result<ChartData> {
    let metadata = table
        .visualization
        .as_ref()
        .context("no Kusto Visualization metadata; add a render operator")?;
    let visualization = prop(metadata, "Visualization")
        .and_then(Value::as_str)
        .context("Visualization type missing")?;
    let kind = kind(table)?;
    let custom_columns = columns.is_some();
    let defaults;
    let columns = match columns {
        Some(columns) => columns,
        None => {
            defaults = default_columns(table)?;
            &defaults
        }
    };
    columns.validate(table, kind)?;
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
    let x = columns.x;
    let mut series: BTreeMap<(Vec<String>, usize), Series> = BTreeMap::new();
    let mut categories = Vec::new();
    for (ordinal, &i) in rows.iter().enumerate() {
        let row = table.rows.get(i).context("chart row out of range")?;
        ensure!(
            row.len() == table.columns.len(),
            "chart row has incorrect column count"
        );
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
        let group = columns
            .series
            .iter()
            .map(|&i| row[i].to_string())
            .collect::<Vec<_>>();
        for &col in &columns.y {
            let label = if group.is_empty() {
                table.columns[col].name.clone()
            } else {
                let label = columns
                    .series
                    .iter()
                    .zip(&group)
                    .map(|(&i, value)| format!("{}={value}", table.columns[i].name))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{label}: {}", table.columns[col].name)
            };
            series
                .entry((group.clone(), col))
                .or_insert_with(|| Series {
                    name: label,
                    points: Vec::new(),
                })
                .points
                .push((xv, number(&row[col])?));
            ensure!(
                series.len() <= 32,
                "more than 32 series; chart refused without dropping series"
            );
        }
    }
    let series: Vec<_> = series
        .into_values()
        .map(|mut series| {
            if matches!(kind, ChartKind::Time | ChartKind::Line) {
                // Kusto summarize and local table sorts do not guarantee ascending X.
                series.points.sort_by(|a, b| a.0.total_cmp(&b.0));
            }
            series
        })
        .collect();
    let mut x_bounds = bounds(series.iter().flat_map(|s| s.points.iter().map(|p| p.0)));
    if kind == ChartKind::Time {
        let first = series[0].points[0].0;
        if series.iter().flat_map(|s| &s.points).all(|p| p.0 == first) {
            x_bounds = [first - 1., first + 1.];
        }
    }
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
            .filter(|_| !custom_columns)
            .and_then(Value::as_str)
            .unwrap_or(&table.columns[x].name)
            .into(),
        y_title: prop(metadata, "YTitle")
            .filter(|_| !custom_columns)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                columns
                    .y
                    .iter()
                    .map(|&i| table.columns[i].name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }),
        series,
        x_bounds,
        y_bounds,
        categories,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{Column, Table};
    use serde_json::json;

    pub(crate) fn time_table() -> Table {
        Table {
            id: 0,
            name: "counts".into(),
            kind: "PrimaryResult".into(),
            columns: [
                ("count_", "long"),
                ("signup_date", "datetime"),
                ("region", "string"),
                ("other_count", "long"),
                ("other_date", "datetime"),
            ]
            .into_iter()
            .map(|(name, kind)| Column {
                name: name.into(),
                kind: kind.into(),
            })
            .collect(),
            rows: vec![
                vec![
                    json!(30),
                    json!("2026-03-01T00:00:00Z"),
                    json!("west"),
                    json!(300),
                    json!("2025-03-01T00:00:00Z"),
                ],
                vec![
                    json!(10),
                    json!("2026-01-01T00:00:00Z"),
                    json!("west"),
                    json!(100),
                    json!("2025-01-01T00:00:00Z"),
                ],
                vec![
                    json!(20),
                    json!("2026-02-01T00:00:00Z"),
                    json!("west"),
                    json!(200),
                    json!("2025-02-01T00:00:00Z"),
                ],
                vec![
                    json!(12),
                    json!("2026-01-01T00:00:00Z"),
                    json!("east"),
                    json!(120),
                    json!("2025-01-01T00:00:00Z"),
                ],
            ],
            visualization: Some(
                json!({"Visualization": "timechart", "YColumns": ["count_"], "Series": ["region"]}),
            ),
        }
    }

    #[test]
    fn timechart_sorts_each_series_not_table_rows() {
        let table = time_table();
        let original_rows = table.rows.clone();
        for rows in [vec![0, 1, 2, 3], vec![3, 2, 1, 0]] {
            let chart = prepare(&table, &rows).unwrap();
            assert_eq!(chart.series.len(), 2);
            let west = chart
                .series
                .iter()
                .find(|s| s.name.contains("west"))
                .unwrap();
            assert_eq!(
                west.points.iter().map(|p| p.1).collect::<Vec<_>>(),
                [10., 20., 30.]
            );
            assert!(west.points.windows(2).all(|p| p[0].0 < p[1].0));
            assert_eq!(
                chart.series.iter().map(|s| s.points.len()).sum::<usize>(),
                4
            );
        }
        let filtered = prepare(&table, &[0, 2]).unwrap();
        assert_eq!(
            filtered.series[0]
                .points
                .iter()
                .map(|p| p.1)
                .collect::<Vec<_>>(),
            [20., 30.]
        );
        assert_eq!(table.rows, original_rows);
    }

    #[test]
    fn column_overrides_replace_metadata_without_mutating_it() {
        let table = time_table();
        let columns = ChartColumns {
            x: 4,
            y: vec![3],
            series: vec![],
        };
        let chart = prepare_with_columns(&table, &[0, 1, 2, 3], Some(&columns)).unwrap();
        assert_eq!(chart.x_title, "other_date");
        assert_eq!(chart.y_title, "other_count");
        assert_eq!(chart.series.len(), 1);
        assert_eq!(
            chart.series[0]
                .points
                .iter()
                .map(|p| p.1)
                .collect::<Vec<_>>(),
            [100., 120., 200., 300.]
        );
        assert!(
            time_label(chart.x_bounds[0], chart.x_bounds[1] - chart.x_bounds[0])
                .starts_with("2025-")
        );
        assert_eq!(
            default_columns(&table).unwrap(),
            ChartColumns {
                x: 1,
                y: vec![0],
                series: vec![2]
            }
        );
        let mut malformed = table.clone();
        malformed.visualization.as_mut().unwrap()["XColumn"] = json!("not a column");
        assert!(prepare(&malformed, &[0]).is_err());
        assert!(prepare_with_columns(&malformed, &[0], Some(&columns)).is_ok());
        malformed.visualization.as_mut().unwrap()["Yaxis"] = json!("log");
        assert!(prepare_with_columns(&malformed, &[0], Some(&columns)).is_err());
    }

    #[test]
    fn mappings_and_malformed_rows_fail_without_panics() {
        let table = time_table();
        for columns in [
            ChartColumns {
                x: 80,
                y: vec![0],
                series: vec![],
            },
            ChartColumns {
                x: 0,
                y: vec![3],
                series: vec![],
            },
            ChartColumns {
                x: 1,
                y: vec![],
                series: vec![],
            },
            ChartColumns {
                x: 1,
                y: vec![2],
                series: vec![],
            },
            ChartColumns {
                x: 1,
                y: vec![0],
                series: vec![0],
            },
            ChartColumns {
                x: 1,
                y: vec![0, 0],
                series: vec![],
            },
            ChartColumns {
                x: 1,
                y: vec![0],
                series: vec![100],
            },
        ] {
            assert!(prepare_with_columns(&table, &[0], Some(&columns)).is_err());
        }
        assert!(prepare(&table, &[]).is_err());
        assert!(prepare(&table, &[99]).is_err());
        let mut malformed = table.clone();
        malformed.rows[0].truncate(2);
        assert!(prepare(&malformed, &[0]).is_err());
        malformed = table.clone();
        malformed.rows[0][0] = Value::Null;
        assert!(prepare(&malformed, &[0]).is_err());
    }

    #[test]
    fn single_timestamp_and_year_labels_are_readable() {
        let table = time_table();
        let data = prepare(&table, &[0]).unwrap();
        assert_eq!(data.x_bounds[1] - data.x_bounds[0], 2.);
        assert_eq!(
            time_label(data.series[0].points[0].0, 365. * 86400.),
            "2026-03-01"
        );
        assert_eq!(
            time_label(data.series[0].points[0].0 + 0.125, 0.5),
            "2026-03-01 00:00:00.125"
        );
    }

    #[test]
    fn line_sorts_x_scatter_keeps_points_and_numeric_groups_are_not_measures() {
        let mut table = time_table();
        table.visualization = Some(
            json!({"Visualization":"linechart", "XColumn":"count_", "YColumns":["other_count"]}),
        );
        let data = prepare(&table, &[0, 1, 2]).unwrap();
        assert_eq!(
            data.series[0].points,
            [(10., 100.), (20., 200.), (30., 300.)]
        );
        table.visualization.as_mut().unwrap()["Visualization"] = json!("scatterchart");
        let scatter = prepare(&table, &[0, 1, 2]).unwrap();
        assert_eq!(
            scatter.series[0].points,
            [(30., 300.), (10., 100.), (20., 200.)]
        );
        table.visualization =
            Some(json!({"Visualization": "timechart", "Series": ["other_count"]}));
        assert_eq!(default_columns(&table).unwrap().y, [0]);
    }

    #[test]
    fn grouping_keys_do_not_merge_null_and_literal_null() {
        let mut table = time_table();
        table.rows[0][2] = Value::Null;
        table.rows[1][2] = json!("null");
        assert_eq!(prepare(&table, &[0, 1]).unwrap().series.len(), 2);
    }
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
