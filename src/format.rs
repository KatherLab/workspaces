//! Machine-readable output helpers (CSV / JSON) shared by `list` and `filesystems`.
//!
//! Both produce one record per row, with the same keys as the table columns.
//! Values are raw (byte counts, ISO timestamps, booleans) rather than the
//! human-friendly strings shown in table mode, so they're easy to consume
//! programmatically.

use std::error::Error;

use crate::cli::OutputFormat;

/// A single field in a record. Holds an already-rendered string; the caller
/// decides how to represent each value (numbers, ISO timestamps, etc.).
/// Empty strings render as an empty CSV field and a JSON `null`.
#[derive(Debug, Clone)]
pub struct Field {
    pub key: &'static str,
    pub value: String,
}

impl Field {
    pub fn new(key: &'static str, value: impl ToString) -> Self {
        Self {
            key,
            value: value.to_string(),
        }
    }
}

/// Emit `records` in the requested format. `table` is a no-op here (callers
/// fall back to their prettytable rendering for that case), so this only
/// handles `csv` and `json`.
pub fn write_records(
    format: &OutputFormat,
    records: &[Vec<Field>],
) -> Result<(), Box<dyn Error>> {
    match format {
        OutputFormat::Table => Ok(()),
        OutputFormat::Csv => write_csv(records),
        OutputFormat::Json => write_json(records),
    }
}

fn write_csv(records: &[Vec<Field>]) -> Result<(), Box<dyn Error>> {
    let mut wtr = csv::Writer::from_writer(std::io::stdout());
    if let Some(first) = records.first() {
        wtr.write_record(first.iter().map(|f| f.key))?;
    }
    for record in records {
        wtr.write_record(record.iter().map(|f| &f.value))?;
    }
    wtr.flush()?;
    Ok(())
}

fn write_json(records: &[Vec<Field>]) -> Result<(), Box<dyn Error>> {
    println!("{}", render_json(records));
    Ok(())
}

/// Render records as a JSON array of objects. Separated from `write_json`
/// so it can be tested without capturing stdout.
fn render_json(records: &[Vec<Field>]) -> String {
    let mut out = String::from("[");
    for (i, record) in records.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        for (j, field) in record.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&json_string(field.key));
            out.push(':');
            out.push_str(&json_value(&field.value));
        }
        out.push('}');
    }
    out.push(']');
    out
}

/// Render a string as a JSON string literal (with surrounding quotes).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Render a field value as a JSON scalar: empty string -> null, otherwise a
/// JSON string literal. (We don't emit bare numbers because every value is
/// already stringified by the caller; JSON string values are unambiguous and
/// safe for consumers to parse and re-type as needed.)
fn json_value(s: &str) -> String {
    if s.is_empty() {
        "null".to_string()
    } else {
        json_string(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::OutputFormat;

    fn sample() -> Vec<Vec<Field>> {
        vec![
            vec![
                Field::new("name", "testws"),
                Field::new("size_bytes", 0),
                Field::new("shares", ""),
            ],
            vec![
                Field::new("name", "bigws"),
                Field::new("size_bytes", 1073741824),
                Field::new("shares", "alice:ro, bob:rw"),
            ],
        ]
    }

    #[test]
    fn csv_has_header_and_rows() {
        let mut buf = Vec::new();
        {
            let mut wtr = csv::Writer::from_writer(&mut buf);
            // mirror write_csv exactly
            let records = sample();
            wtr.write_record(records[0].iter().map(|f| f.key)).unwrap();
            for record in &records {
                wtr.write_record(record.iter().map(|f| &f.value)).unwrap();
            }
            wtr.flush().unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(
            out,
            "name,size_bytes,shares\n\
             testws,0,\n\
             bigws,1073741824,\"alice:ro, bob:rw\"\n"
        );
    }

    #[test]
    fn json_is_valid_array() {
        let records = sample();
        let out = render_json(&records);
        // Structural checks (avoids pulling in a JSON parser dependency).
        assert!(out.starts_with('[') && out.ends_with(']'));
        assert_eq!(out.matches('{').count(), 2);
        assert_eq!(out.matches('}').count(), 2);
        // empty shares field renders as null
        assert!(out.contains("\"shares\":null"));
        // populated shares field is a quoted string
        assert!(out.contains("\"shares\":\"alice:ro, bob:rw\""));
        // numbers come through as quoted strings
        assert!(out.contains("\"size_bytes\":\"0\""));
    }

    #[test]
    fn empty_value_becomes_json_null() {
        assert_eq!(json_value(""), "null");
        assert_eq!(json_value("x"), "\"x\"");
    }

    #[test]
    fn json_escapes_special_chars() {
        assert_eq!(json_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_string("line\nbreak"), "\"line\\nbreak\"");
    }

    #[test]
    fn write_records_table_is_noop() {
        // Table format should produce no output here (caller renders the table).
        write_records(&OutputFormat::Table, &sample()).unwrap();
    }
}

