//! Commands return results; this module renders them, as text or JSON
//! (`--json`), on stdout. Errors go through here too, on stderr. Progress is
//! `log` on stderr and never mixes into stdout.

use miniserde::json::{Number, Object};

use crate::error::Error;
use crate::json::{self, Json};

/// Every JSON document's `version`: 1 until the first release, then bumped
/// when a field changes meaning or goes away.
const VERSION: u64 = 1;

#[derive(Clone, Copy)]
pub(crate) enum Format {
    Human,
    Json,
}

trait Render {
    /// Text for people; empty prints nothing.
    fn human(&self) -> String;
    /// The documented `--json` shape; `Null` prints nothing.
    fn json(&self) -> Json;
}

pub(crate) enum Outcome {
    Done,
    /// The command wrote its own output.
    Streamed,
}

pub(crate) fn emit(format: Format, result: &Outcome) {
    match format {
        Format::Human => {
            let text = result.human();
            if !text.is_empty() {
                println!("{text}");
            }
        }
        Format::Json => {
            let value = result.json();
            if !matches!(value, Json::Null) {
                println!("{}", json::stringify(&value));
            }
        }
    }
}

pub(crate) fn emit_error(format: Format, error: &Error) {
    match format {
        Format::Human => log::error!("{error}"),
        Format::Json => eprintln!("{}", error_json(error)),
    }
}

fn error_json(error: &Error) -> String {
    let hint = error.hint().map_or(Json::Null, json::string);
    let value = json::object([
        ("error", json::string(error.message())),
        ("hint", hint),
    ]);
    json::stringify(&value)
}

fn document<const N: usize>(fields: [(&str, Json); N]) -> Json {
    let mut object = Object::new();
    object.insert("version".into(), Json::Number(Number::U64(VERSION)));
    for (key, value) in fields {
        object.insert(key.into(), value);
    }
    Json::Object(object)
}

impl Render for Outcome {
    fn human(&self) -> String {
        String::new()
    }

    fn json(&self) -> Json {
        match self {
            Self::Done => document([("ok", Json::Bool(true))]),
            Self::Streamed => Json::Null,
        }
    }
}

/// Rows padded to each column's widest cell, two spaces apart, indented
/// under their section; nothing without rows below the header.
fn table(rows: &[Vec<String>]) -> Vec<String> {
    if rows.len() < 2 {
        return Vec::new();
    }
    let mut widths: Vec<usize> = Vec::new();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            let width = cell.chars().count();
            match widths.get_mut(index) {
                Some(widest) => *widest = (*widest).max(width),
                None => widths.push(width),
            }
        }
    }
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect();
            format!("  {}", cells.join("  ").trim_end())
        })
        .collect()
}

fn cells<const N: usize>(values: [&str; N]) -> Vec<String> {
    values.map(String::from).to_vec()
}

/// Each app's hosts and the credential each one gets, by name only.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    #[expect(clippy::cast_precision_loss, reason = "display only")]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::{Outcome, Render, error_json};
    use crate::error::Error;

    fn to_string(value: &crate::json::Json) -> String {
        crate::json::stringify(value)
    }

    #[test]
    fn done_and_streamed_render_nothing_for_people() {
        assert_eq!(Outcome::Done.human(), "");
        assert_eq!(
            to_string(&Outcome::Done.json()),
            r#"{"ok":true,"version":1}"#
        );
        assert_eq!(Outcome::Streamed.human(), "");
        assert!(matches!(Outcome::Streamed.json(), crate::json::Json::Null));
    }

    #[test]
    fn errors_as_json_keep_the_hint_apart() {
        let hinted = Error::with_hint("the vault isn't running", "run up");
        assert_eq!(
            error_json(&hinted),
            r#"{"error":"the vault isn't running","hint":"run up"}"#
        );
        assert_eq!(
            error_json(&Error::new("plain")),
            r#"{"error":"plain","hint":null}"#
        );
    }
}
