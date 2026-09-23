use anyhow::{Context, Result, bail, ensure};
use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{cmp::Ordering, str::FromStr};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Column {
    #[serde(rename = "ColumnName")]
    pub name: String,
    #[serde(rename = "ColumnType")]
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Table {
    #[serde(rename = "TableId", default)]
    pub id: i64,
    #[serde(rename = "TableName", default)]
    pub name: String,
    #[serde(rename = "TableKind", default)]
    pub kind: String,
    #[serde(rename = "Columns")]
    pub columns: Vec<Column>,
    #[serde(rename = "Rows")]
    pub rows: Vec<Vec<Value>>,
    #[serde(skip)]
    pub visualization: Option<Value>,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct QueryResult {
    pub tables: Vec<Table>,
    pub metadata: Vec<Table>,
    pub partial: bool,
    pub diagnostics: Vec<String>,
    pub client_request_id: String,
    pub activity_id: Option<String>,
}

pub fn unpack(value: &Value) -> Value {
    value
        .as_str()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| value.clone())
}
pub fn text(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
pub fn numeric_type(kind: &str) -> bool {
    matches!(
        kind,
        "int"
            | "long"
            | "real"
            | "decimal"
            | "System.Int32"
            | "System.Int64"
            | "System.Double"
            | "System.Decimal"
    )
}

fn table(value: &Value) -> Result<(Table, Vec<Value>)> {
    let mut value = value.clone();
    let mut errors = Vec::new();
    if let Some(rows) = value["Rows"].as_array_mut() {
        for row in rows.iter() {
            if row.is_array() {
                continue;
            }
            let inline = row["OneApiErrors"]
                .as_array()
                .filter(|a| !a.is_empty())
                .context("result contains a malformed row")?;
            errors.extend(inline.iter().cloned());
        }
        rows.retain(Value::is_array);
    }
    // V1 management responses may include both the Kusto ColumnType and CLR DataType.
    if let Some(columns) = value["Columns"].as_array_mut() {
        for column in columns {
            if column.get("ColumnType").is_none() {
                column["ColumnType"] = column["DataType"].clone();
            }
        }
    }
    let table: Table = serde_json::from_value(value).context("malformed result table")?;
    ensure!(
        table.rows.iter().all(|r| r.len() == table.columns.len()),
        "result row width does not match schema"
    );
    Ok((table, errors))
}

pub fn decode_v2(bytes: &[u8], max_rows: usize) -> Result<QueryResult> {
    let value: Value = serde_json::from_slice(bytes).context("invalid Kusto response JSON")?;
    if let Some(e) = value.get("error") {
        bail!("Kusto error: {}", text(e));
    }
    let frames = value
        .as_array()
        .context("query response must be a V2 frame array")?;
    ensure!(
        frames.first().and_then(|v| v["FrameType"].as_str()) == Some("DataSetHeader"),
        "missing V2 dataset header"
    );
    ensure!(
        frames[0]["IsProgressive"] == false,
        "progressive response unsupported; requested nonprogressive"
    );
    ensure!(
        frames[0]["Version"] == "v2.0",
        "unsupported Kusto protocol version"
    );
    ensure!(
        frames.last().and_then(|v| v["FrameType"].as_str()) == Some("DataSetCompletion"),
        "incomplete V2 response: missing completion"
    );
    let mut result = QueryResult::default();
    let mut remaining = max_rows;
    ensure!(
        frames
            .iter()
            .filter(|f| f["FrameType"] == "DataSetHeader")
            .count()
            == 1
            && frames
                .iter()
                .filter(|f| f["FrameType"] == "DataSetCompletion")
                .count()
                == 1,
        "duplicate dataset header/completion"
    );
    for frame in frames {
        match frame["FrameType"].as_str() {
            Some("DataSetHeader") => {}
            Some("DataTable") => {
                let (mut t, inline_errors) = table(frame)?;
                if !inline_errors.is_empty() {
                    result.partial = true;
                    for error in inline_errors {
                        result.diagnostics.push(format!(
                            "row-level query failure: {}",
                            error_description(&error)
                        ));
                    }
                }
                if t.kind == "PrimaryResult" {
                    if t.rows.len() > remaining {
                        t.rows.truncate(remaining);
                        result.partial = true;
                        result.diagnostics.push(format!(
                            "local row safety limit ({max_rows}) reached; results are partial"
                        ));
                    }
                    remaining = remaining.saturating_sub(t.rows.len());
                    result.tables.push(t);
                } else {
                    if t.kind == "QueryCompletionInformation" {
                        let level = t
                            .columns
                            .iter()
                            .position(|c| c.name == "Level" || c.name == "Severity");
                        let payload = t
                            .columns
                            .iter()
                            .position(|c| c.name == "Payload" || c.name == "StatusDescription");
                        for row in &t.rows {
                            let p = payload.map(|i| unpack(&row[i]));
                            let failed = level.and_then(|i| row[i].as_i64()).is_some_and(|n| n < 4)
                                || p.as_ref().is_some_and(completion_failed);
                            if failed {
                                result.partial = true;
                                result.diagnostics.push(format!(
                                    "query completion failure: {}",
                                    p.as_ref().map(text).unwrap_or_else(|| format!("{row:?}"))
                                ));
                            }
                        }
                    }
                    result.metadata.push(t);
                }
            }
            Some("DataSetCompletion") => {
                ensure!(
                    frame["HasErrors"].is_boolean() && frame["Cancelled"].is_boolean(),
                    "malformed dataset completion flags"
                );
                if frame["HasErrors"] == true
                    || frame["Cancelled"] == true
                    || frame["OneApiErrors"]
                        .as_array()
                        .is_some_and(|a| !a.is_empty())
                {
                    result.partial = true;
                    result.diagnostics.push(format!(
                        "dataset completion: HasErrors={}, Cancelled={}",
                        frame["HasErrors"], frame["Cancelled"]
                    ));
                    if let Some(errors) = frame["OneApiErrors"].as_array() {
                        for error in errors {
                            result.diagnostics.push(error_description(error));
                        }
                    }
                }
            }
            other => bail!("unsupported V2 frame {other:?}"),
        }
    }
    for t in &result.metadata {
        if t.kind != "QueryProperties" {
            continue;
        }
        let key = t.columns.iter().position(|c| c.name == "Key");
        let val = t.columns.iter().position(|c| c.name == "Value");
        let id = t.columns.iter().position(|c| c.name == "TableId");
        if let (Some(key), Some(val), Some(id)) = (key, val, id) {
            for row in &t.rows {
                if row[key] == "Visualization" {
                    let table_id = row[id]
                        .as_i64()
                        .or_else(|| row[id].as_str().and_then(|s| s.parse().ok()));
                    if let Some(target) = result.tables.iter_mut().find(|t| Some(t.id) == table_id)
                    {
                        target.visualization = Some(unpack(&row[val]));
                    }
                }
            }
        }
    }
    Ok(result)
}

fn completion_failed(v: &Value) -> bool {
    match v {
        Value::Object(map) => map.iter().any(|(key, val)| {
            (matches!(
                key.as_str(),
                "HasErrors" | "Cancelled" | "IsPartial" | "IsTruncated"
            ) && val == true)
                || (key == "StatusCode" && val.as_i64().is_some_and(|n| n != 0))
                || (key == "Severity" && val.as_i64().is_some_and(|n| n < 4))
                || (matches!(key.as_str(), "Errors" | "OneApiErrors")
                    && val.as_array().is_some_and(|a| !a.is_empty()))
                || completion_failed(val)
        }),
        Value::Array(a) => a.iter().any(completion_failed),
        _ => false,
    }
}
fn error_description(value: &Value) -> String {
    let e = value.get("error").unwrap_or(value);
    let code = e["code"].as_str().unwrap_or("KustoError");
    let message = e["@message"]
        .as_str()
        .or(e["message"].as_str())
        .unwrap_or("query failed");
    format!("{code}: {message}")
}

pub fn decode_mgmt(bytes: &[u8]) -> Result<Vec<Table>> {
    let v: Value = serde_json::from_slice(bytes)?;
    if let Some(e) = v.get("error") {
        bail!("metadata request failed: {e}");
    }
    let tables = v["Tables"]
        .as_array()
        .context("management response has no Tables")?
        .iter()
        .map(|v| {
            let (table, errors) = table(v)?;
            ensure!(
                errors.is_empty(),
                "management result contains inline errors: {}",
                errors
                    .iter()
                    .map(error_description)
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            Ok(table)
        })
        .collect::<Result<Vec<_>>>()?;
    for t in &tables {
        if let Some(i) = t.columns.iter().position(|c| c.name == "Severity") {
            ensure!(
                !t.rows.iter().any(|r| r[i].as_i64().is_some_and(|n| n < 4)),
                "management completion reported a failure"
            );
        }
    }
    Ok(tables)
}

#[derive(Clone, Default, Debug)]
pub struct ViewSpec {
    pub sort: Option<(usize, bool)>,
    pub filter: String,
    pub column_filter: Option<(usize, FilterOp, String)>,
}
#[derive(Clone, Copy, Debug)]
pub enum FilterOp {
    Eq,
    Lt,
    Gt,
    Contains,
}

fn decimal(v: &Value) -> Option<BigDecimal> {
    BigDecimal::from_str(&text(v)).ok()
}
fn timespan(value: &str) -> Option<BigDecimal> {
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |v| (true, v));
    let parts: Vec<_> = value.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let (days, hours) = parts[0].split_once('.').unwrap_or(("0", parts[0]));
    let days: u64 = days.parse().ok()?;
    let hours: u64 = hours.parse().ok()?;
    let minutes: u64 = parts[1].parse().ok()?;
    let seconds = BigDecimal::from_str(parts[2]).ok()?;
    if hours > 23 || minutes > 59 || !(BigDecimal::from(0)..BigDecimal::from(60)).contains(&seconds)
    {
        return None;
    }
    let whole = days
        .checked_mul(86400)?
        .checked_add(hours * 3600 + minutes * 60)?;
    let duration = BigDecimal::from(whole) + seconds;
    Some(if negative { -duration } else { duration })
}
pub fn compare(a: &Value, b: &Value, kind: &str) -> Ordering {
    if a.is_null() || b.is_null() {
        return (!a.is_null()).cmp(&!b.is_null());
    }
    if numeric_type(kind)
        && let (Some(a), Some(b)) = (decimal(a), decimal(b))
    {
        return a.cmp(&b);
    }
    if matches!(kind, "timespan" | "System.TimeSpan")
        && let (Some(a), Some(b)) = (timespan(&text(a)), timespan(&text(b)))
    {
        return a.cmp(&b);
    }
    if matches!(kind, "datetime" | "System.DateTime")
        && let (Ok(a), Ok(b)) = (
            chrono::DateTime::parse_from_rfc3339(&text(a)),
            chrono::DateTime::parse_from_rfc3339(&text(b)),
        )
    {
        return a.cmp(&b);
    }
    if matches!(kind, "bool" | "System.Boolean") {
        return a.as_bool().cmp(&b.as_bool());
    }
    text(a).cmp(&text(b))
}
impl ViewSpec {
    pub fn indices(&self, table: &Table) -> Result<Vec<usize>> {
        if let Some((i, _)) = self.sort {
            ensure!(i < table.columns.len(), "sort column out of range");
        }
        if let Some((i, op, value)) = &self.column_filter {
            let col = table
                .columns
                .get(*i)
                .context("filter column out of range")?;
            if !matches!(op, FilterOp::Contains) && value != "null" {
                if numeric_type(&col.kind) {
                    BigDecimal::from_str(value).context("numeric filter requires a number")?;
                }
                if matches!(col.kind.as_str(), "datetime" | "System.DateTime") {
                    chrono::DateTime::parse_from_rfc3339(value)
                        .context("datetime filter requires RFC3339")?;
                }
                if matches!(col.kind.as_str(), "timespan" | "System.TimeSpan") {
                    timespan(value)
                        .context("timespan filter requires [-][days.]HH:MM:SS[.fraction]")?;
                }
                if matches!(col.kind.as_str(), "bool" | "System.Boolean") {
                    ensure!(
                        value == "true" || value == "false",
                        "boolean filter requires true or false"
                    );
                }
            }
        }
        let needle = self.filter.to_lowercase();
        let mut rows: Vec<usize> = table
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                (needle.is_empty() || row.iter().any(|v| text(v).to_lowercase().contains(&needle)))
                    && self.column_filter.as_ref().is_none_or(|(i, op, value)| {
                        let expected = if value == "null" {
                            Value::Null
                        } else if matches!(
                            table.columns[*i].kind.as_str(),
                            "bool" | "System.Boolean"
                        ) {
                            serde_json::from_str(value).unwrap_or(Value::String(value.clone()))
                        } else {
                            Value::String(value.clone())
                        };
                        match op {
                            FilterOp::Contains => text(&row[*i]).contains(value),
                            FilterOp::Eq => {
                                compare(&row[*i], &expected, &table.columns[*i].kind)
                                    == Ordering::Equal
                            }
                            FilterOp::Lt => {
                                compare(&row[*i], &expected, &table.columns[*i].kind)
                                    == Ordering::Less
                            }
                            FilterOp::Gt => {
                                compare(&row[*i], &expected, &table.columns[*i].kind)
                                    == Ordering::Greater
                            }
                        }
                    })
            })
            .map(|(i, _)| i)
            .collect();
        if let Some((col, descending)) = self.sort {
            rows.sort_by(|a, b| {
                let order = compare(
                    &table.rows[*a][col],
                    &table.rows[*b][col],
                    &table.columns[col].kind,
                );
                if descending { order.reverse() } else { order }
            });
        }
        Ok(rows)
    }
}

#[cfg(test)]
pub fn fixture() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!([
        {"FrameType":"DataSetHeader","Version":"v2.0","IsProgressive":false},
        {"FrameType":"DataTable","TableId":0,"TableKind":"PrimaryResult","TableName":"PrimaryResult","Columns":[{"ColumnName":"x","ColumnType":"long"},{"ColumnName":"v","ColumnType":"decimal"}],"Rows":[[9007199254740993_i64,"-2.125"],[2,"0.50"],[2,null]]},
        {"FrameType":"DataSetCompletion","HasErrors":false,"Cancelled":false}
    ])).unwrap()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_sort_filter_is_stable_and_exact() {
        let r = decode_v2(&fixture(), 10).unwrap();
        let t = &r.tables[0];
        assert_eq!(
            ViewSpec {
                sort: Some((0, false)),
                ..Default::default()
            }
            .indices(t)
            .unwrap(),
            [1, 2, 0]
        );
        assert_eq!(
            ViewSpec {
                column_filter: Some((1, FilterOp::Lt, "0".into())),
                ..Default::default()
            }
            .indices(t)
            .unwrap(),
            [0, 2]
        );
        assert_eq!(t.rows[0][0].as_i64(), Some(9007199254740993));
        assert_eq!(t.rows.len(), 3);
    }
    #[test]
    fn http_200_completion_errors_and_limits_are_partial() {
        let mut v: Value = serde_json::from_slice(&fixture()).unwrap();
        v[2]["HasErrors"] = true.into();
        assert!(
            decode_v2(&serde_json::to_vec(&v).unwrap(), 10)
                .unwrap()
                .partial
        );
        assert!(decode_v2(&fixture(), 1).unwrap().partial);
        v[2]["HasErrors"] = false.into();
        v.as_array_mut().unwrap().insert(2, serde_json::json!({"FrameType":"DataTable","TableKind":"QueryCompletionInformation","Columns":[{"ColumnName":"Level","ColumnType":"int"},{"ColumnName":"Payload","ColumnType":"dynamic"}],"Rows":[[2,{"StatusCode":1,"StatusDescription":"truncated"}]]}));
        let r = decode_v2(&serde_json::to_vec(&v).unwrap(), 10).unwrap();
        assert!(r.partial);
        assert!(r.diagnostics[0].contains("truncated"));
        assert!(decode_v2(br#"{"Tables":[]}"#, 10).is_err());
        assert!(decode_v2(br#"[]"#, 10).is_err());
    }
    #[test]
    fn management_accepts_kusto_and_clr_column_types_together() {
        let result = decode_mgmt(br#"{"Tables":[{"Columns":[{"ColumnName":"DatabaseName","ColumnType":"string","DataType":"String"}],"Rows":[["example"]]}]}"#).unwrap();
        assert_eq!(result[0].columns[0].kind, "string");
        assert_eq!(result[0].rows[0][0], "example");
        let legacy = decode_mgmt(br#"{"Tables":[{"Columns":[{"ColumnName":"DatabaseName","DataType":"String"}],"Rows":[]}]}"#).unwrap();
        assert_eq!(legacy[0].columns[0].kind, "String");
    }
    #[test]
    fn all_primary_tables_visualization_and_nested_completion_are_preserved() {
        let mut v: Value = serde_json::from_slice(&fixture()).unwrap();
        let frames = v.as_array_mut().unwrap();
        let mut second = frames[1].clone();
        second["TableId"] = 7.into();
        second["TableName"] = "Other".into();
        frames.insert(2, second);
        frames.insert(3, serde_json::json!({"FrameType":"DataTable","TableKind":"QueryProperties","Columns":[{"ColumnName":"TableId","ColumnType":"int"},{"ColumnName":"Key","ColumnType":"string"},{"ColumnName":"Value","ColumnType":"dynamic"}],"Rows":[[7,"Visualization","{\"Visualization\":\"linechart\"}"]]}));
        let r = decode_v2(&serde_json::to_vec(&v).unwrap(), 20).unwrap();
        assert_eq!(r.tables.len(), 2);
        assert!(r.tables[0].visualization.is_none());
        assert_eq!(
            r.tables[1].visualization.as_ref().unwrap()["Visualization"],
            "linechart"
        );
        let frames = v.as_array_mut().unwrap();
        frames.insert(4, serde_json::json!({"FrameType":"DataTable","TableKind":"QueryCompletionInformation","Columns":[{"ColumnName":"Level","ColumnType":"int"},{"ColumnName":"Payload","ColumnType":"dynamic"}],"Rows":[[4,"{\"StatusCode\":1,\"Info\":{\"IsTruncated\":true}}"]]}));
        assert!(
            decode_v2(&serde_json::to_vec(&v).unwrap(), 20)
                .unwrap()
                .partial
        );
    }
    #[test]
    fn malformed_completion_and_progressive_are_not_success() {
        let mut v: Value = serde_json::from_slice(&fixture()).unwrap();
        v[2].as_object_mut().unwrap().remove("HasErrors");
        assert!(decode_v2(&serde_json::to_vec(&v).unwrap(), 10).is_err());
        v[0]["IsProgressive"] = true.into();
        assert!(decode_v2(&serde_json::to_vec(&v).unwrap(), 10).is_err());
        assert!(decode_mgmt(br#"{"Tables":[{"Columns":[{"ColumnName":"Severity","ColumnType":"int"}],"Rows":[[2]]}]}"#).is_err());
    }
    #[test]
    fn inline_one_api_errors_retain_rows_and_mark_partial() {
        let mut v: Value = serde_json::from_slice(&fixture()).unwrap();
        v[0]["ErrorReportingPlacement"] = "InData".into();
        v[1]["Rows"].as_array_mut().unwrap().push(serde_json::json!({"OneApiErrors":[{"error":{"code":"LimitsExceeded","@message":"too many records"}}]}));
        let r = decode_v2(&serde_json::to_vec(&v).unwrap(), 10).unwrap();
        assert!(r.partial);
        assert_eq!(r.tables[0].rows.len(), 3);
        assert!(r.diagnostics[0].contains("too many records"));
        v[1]["Rows"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"unexpected":"not a row"}));
        assert!(decode_v2(&serde_json::to_vec(&v).unwrap(), 10).is_err());
    }
    #[test]
    fn exact_typed_temporal_boolean_and_null_ordering() {
        use serde_json::json;
        assert_eq!(
            compare(&json!("-0.00000000000000000001"), &json!("0"), "decimal"),
            Ordering::Less
        );
        assert_eq!(
            compare(&json!("2.00:00:00"), &json!("10.00:00:00"), "timespan"),
            Ordering::Less
        );
        assert_eq!(
            compare(&json!("-00:00:00.1"), &json!("00:00:00"), "timespan"),
            Ordering::Less
        );
        assert_eq!(
            compare(
                &json!("2026-01-01T03:00:00+03:00"),
                &json!("2026-01-01T00:00:00Z"),
                "datetime"
            ),
            Ordering::Equal
        );
        assert_eq!(compare(&json!(false), &json!(true), "bool"), Ordering::Less);
        assert_eq!(compare(&Value::Null, &json!(-10), "long"), Ordering::Less);
        let t = Table {
            id: 0,
            name: String::new(),
            kind: "PrimaryResult".into(),
            columns: vec![Column {
                name: "b".into(),
                kind: "bool".into(),
            }],
            rows: vec![vec![json!(true)]],
            visualization: None,
        };
        assert!(
            ViewSpec {
                column_filter: Some((0, FilterOp::Eq, "oops".into())),
                ..Default::default()
            }
            .indices(&t)
            .is_err()
        );
    }
}
