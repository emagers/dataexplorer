use crate::{
    config::atomic_write,
    model::{QueryResult, Table, text},
};
use anyhow::{Result, ensure};
use clap::ValueEnum;
use serde_json::json;
use std::{io::Write, path::Path};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Format {
    Csv,
    Json,
    Jsonl,
}

pub fn write<W: Write>(
    mut writer: W,
    result: &QueryResult,
    table: &Table,
    rows: &[usize],
    format: Format,
    accept_partial: bool,
) -> Result<()> {
    ensure!(
        !result.partial || accept_partial,
        "results are PARTIAL; use --accept-partial to export explicitly"
    );
    ensure!(
        rows.iter().all(|i| *i < table.rows.len()),
        "export row index out of range"
    );
    match format {
        Format::Csv => {
            let mut csv = csv::Writer::from_writer(writer);
            csv.write_record(table.columns.iter().map(|c| &c.name))?;
            for &i in rows {
                csv.write_record(
                    table.rows[i]
                        .iter()
                        .map(|v| if v.is_null() { String::new() } else { text(v) }),
                )?;
            }
            csv.flush()?;
        }
        Format::Json => {
            let selected: Vec<_> = rows.iter().map(|i| &table.rows[*i]).collect();
            serde_json::to_writer_pretty(
                &mut writer,
                &json!({
                    "version":1,"table_id":table.id,"table_name":table.name,"columns":table.columns,
                    "rows":selected,"partial":result.partial,"diagnostics":result.diagnostics,
                    "client_request_id":result.client_request_id,"activity_id":result.activity_id,
                    "visualization":table.visualization
                }),
            )?;
            writeln!(writer)?;
        }
        Format::Jsonl => {
            // Positional arrays preserve duplicate column names and exact scalar representation.
            for &i in rows {
                serde_json::to_writer(&mut writer, &table.rows[i])?;
                writeln!(writer)?;
            }
        }
    }
    Ok(())
}

pub fn file(
    path: &Path,
    overwrite: bool,
    result: &QueryResult,
    table: &Table,
    rows: &[usize],
    format: Format,
    accept_partial: bool,
) -> Result<()> {
    atomic_write(path, overwrite, |w| {
        write(w, result, table, rows, format, accept_partial)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{decode_v2, fixture};
    #[test]
    fn csv_escaping_jsonl_arrays_and_failed_atomic_replace() {
        let mut r = decode_v2(&fixture(), 10).unwrap();
        r.tables[0].rows = vec![vec![
            serde_json::json!("comma,\"quote\"\nnewline"),
            serde_json::Value::Null,
        ]];
        let mut csv = Vec::new();
        write(&mut csv, &r, &r.tables[0], &[0], Format::Csv, false).unwrap();
        assert_eq!(
            String::from_utf8(csv).unwrap(),
            "x,v\n\"comma,\"\"quote\"\"\nnewline\",\n"
        );
        let mut jsonl = Vec::new();
        write(&mut jsonl, &r, &r.tables[0], &[0], Format::Jsonl, false).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&jsonl).unwrap(),
            serde_json::json!(r.tables[0].rows[0])
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        std::fs::write(&path, b"original").unwrap();
        assert!(file(&path, false, &r, &r.tables[0], &[0], Format::Json, false).is_err());
        r.partial = true;
        assert!(file(&path, true, &r, &r.tables[0], &[0], Format::Json, false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn exports_preserve_schema_and_numbers_and_guard_partial() {
        let mut r = decode_v2(&fixture(), 10).unwrap();
        let mut bytes = Vec::new();
        write(
            &mut bytes,
            &r,
            &r.tables[0],
            &[0, 1, 2],
            Format::Json,
            false,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["columns"][0]["ColumnType"], "long");
        assert_eq!(v["rows"][0][0].as_i64(), Some(9007199254740993));
        assert_eq!(v["rows"][0][1], "-2.125");
        bytes.clear();
        write(&mut bytes, &r, &r.tables[0], &[1], Format::Csv, false).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "x,v\n2,0.50\n");
        r.partial = true;
        assert!(write(Vec::new(), &r, &r.tables[0], &[0], Format::Jsonl, false).is_err());
        assert!(write(Vec::new(), &r, &r.tables[0], &[0], Format::Jsonl, true).is_ok());
    }
}
