//! Reviewed synthetic-fixture diagnostic, independent of Excel/Windows.
//! Compile/runtime evidence is separate from accepting an engine's feature fidelity.
//! Reads input immutably, edits one explicit typed-string cell, creates a new
//! output only, inventories package changes, and rereads the candidate artifact.
//! Source size/ZIP admission caps are diagnostic bounds, not total RSS guarantees.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::{File, OpenOptions},
    io::{self, Cursor, Read, Write},
    path::{Path, PathBuf},
};

type ProbeResult<T> = Result<T, Box<dyn std::error::Error>>;
const INPUT_CAP: u64 = 16 * 1024 * 1024;
const OUTPUT_CAP: u64 = 32 * 1024 * 1024;
const ENTRY_CAP: u64 = 16 * 1024 * 1024;
const EXPANDED_CAP: u64 = 64 * 1024 * 1024;
const ENTRY_COUNT_CAP: usize = 2048;
const FIXTURE_MARKER: &str = "probe/fixture-id.txt";
const FIXTURE_ID: &[u8] = b"headless-excel-editor-synthetic-v1\n";
const OPAQUE_PARTS: &[&str] = &[
    "customXml/item1.xml",
    "xl/opaque/workbook-marker.xml",
    "xl/opaque/edited-sheet-marker.xml",
    "xl/opaque/untouched-sheet-marker.xml",
];

fn invalid(message: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::new(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
}

fn read_bounded(path: &Path, cap: u64) -> ProbeResult<Vec<u8>> {
    let file = File::open(path)?;
    if file.metadata()?.len() > cap {
        return Err(invalid("file exceeds diagnostic source/output size cap"));
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(invalid("file grew beyond size cap during read"));
    }
    Ok(bytes)
}

fn hash(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut encoded, "{byte:02x}").expect("format into String");
    }
    encoded
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Part {
    bytes: u64,
    sha256: String,
}

fn inventory(bytes: &[u8], require_fixture: bool) -> ProbeResult<BTreeMap<String, Part>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    if archive.len() > ENTRY_COUNT_CAP {
        return Err(invalid("ZIP entry count cap exceeded"));
    }
    let mut total = 0u64;
    let mut actual_total = 0u64;
    let mut parts = BTreeMap::new();
    let mut marker_valid = false;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        if name.starts_with('/')
            || name.contains('\\')
            || name.split('/').any(|segment| segment == "..")
        {
            return Err(invalid("non-canonical package entry path"));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| invalid("ZIP size sum overflow"))?;
        if entry.size() > ENTRY_CAP || total > EXPANDED_CAP {
            return Err(invalid("ZIP declared expansion cap exceeded"));
        }
        let mut data = Vec::new();
        let remaining = EXPANDED_CAP
            .checked_sub(actual_total)
            .ok_or_else(|| invalid("ZIP actual expansion accounting exceeded cap"))?;
        let allowance = ENTRY_CAP.min(remaining);
        entry.take(allowance + 1).read_to_end(&mut data)?;
        if data.len() as u64 > allowance {
            return Err(invalid("ZIP actual entry/aggregate expansion cap exceeded"));
        }
        actual_total = actual_total
            .checked_add(data.len() as u64)
            .ok_or_else(|| invalid("ZIP actual expansion sum overflow"))?;
        if name == FIXTURE_MARKER {
            marker_valid = data == FIXTURE_ID;
        }
        if parts
            .insert(
                name,
                Part {
                    bytes: data.len() as u64,
                    sha256: hash(&data),
                },
            )
            .is_some()
        {
            return Err(invalid("duplicate ZIP entry name"));
        }
    }
    if require_fixture && !marker_valid {
        return Err(invalid(
            "input is not the private synthetic fixture (marker absent/incorrect)",
        ));
    }
    Ok(parts)
}

/// Validated bounded coordinates avoid passing unchecked strings into library
/// coordinate conversions. umya tuple order is (column, row), one-based.
fn coordinate(text: &str) -> ProbeResult<(u32, u32)> {
    let mut column = 0u32;
    let mut row = 0u32;
    let mut digits = false;
    for byte in text.bytes() {
        if byte.is_ascii_uppercase() && !digits {
            column = column
                .checked_mul(26)
                .and_then(|value| value.checked_add(u32::from(byte - b'A' + 1)))
                .ok_or_else(|| invalid("column overflow"))?;
        } else if byte.is_ascii_digit() {
            digits = true;
            row = row
                .checked_mul(10)
                .and_then(|value| value.checked_add(u32::from(byte - b'0')))
                .ok_or_else(|| invalid("row overflow"))?;
        } else {
            return Err(invalid("cell must be an uppercase single A1 coordinate"));
        }
    }
    if !(1..=16384).contains(&column) || !(1..=1048576).contains(&row) {
        return Err(invalid("cell outside XLSX coordinate bounds"));
    }
    Ok((column, row))
}

fn snapshot(sheet: &umya_spreadsheet::Worksheet, position: (u32, u32)) -> Value {
    match sheet.cell(position) {
        Some(cell) => {
            json!({"present":true, "value":cell.value().into_owned(), "formula":cell.formula()})
        }
        None => json!({"present":false}),
    }
}

fn new_output_path(input: &Path, output: &Path) -> ProbeResult<PathBuf> {
    let name = output
        .file_name()
        .ok_or_else(|| invalid("output must name a new file"))?;
    if output.extension().and_then(|value| value.to_str()) != Some("xlsx") {
        return Err(invalid("this diagnostic emits .xlsx only"));
    }
    let parent = output
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let resolved = parent.canonicalize()?.join(name);
    if resolved == input.canonicalize()? {
        return Err(invalid("output aliases the immutable input path"));
    }
    // create_new at actual open is the final guard against existing files,
    // hard links and symlinks; this existence check is not used as authorization.
    Ok(resolved)
}

fn run(
    input: &Path,
    output: &Path,
    sheet_name: &str,
    cell: &str,
    new_text: &str,
    lazy: bool,
) -> ProbeResult<Value> {
    if new_text.len() > 1024 {
        return Err(invalid("replacement text cap exceeded"));
    }
    let position = coordinate(cell)?;
    let source = read_bounded(input, INPUT_CAP)?;
    let input_parts = inventory(&source, true)?;
    let output = new_output_path(input, output)?;
    let mut book =
        umya_spreadsheet::reader::xlsx::read_reader(Cursor::new(source.as_slice()), !lazy)?;
    // Mutable lookup materializes only the selected sheet in lazy mode.
    let before;
    {
        let sheet = book.sheet_by_name_mut(sheet_name)?;
        before = snapshot(sheet, position);
        sheet.cell_mut(position).set_value_string(new_text);
    }
    let after_edit = snapshot(book.sheet_by_name(sheet_name)?, position);
    // Do not use path writer's predictable .xlsxtmp/rename path. Open exclusively
    // and pass this owned file to the arbitrary-writer API; never overwrite.
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)?;
    umya_spreadsheet::writer::xlsx::write_writer(&book, &mut destination)?;
    destination.flush()?;
    destination.sync_all()?;
    drop(destination);
    let emitted = read_bounded(&output, OUTPUT_CAP)?;
    let output_parts = inventory(&emitted, false)?;
    let readback =
        umya_spreadsheet::reader::xlsx::read_reader(Cursor::new(emitted.as_slice()), true)?;
    let roundtrip = snapshot(readback.sheet_by_name(sheet_name)?, position);
    let edited_value_matches = roundtrip.get("value").and_then(Value::as_str) == Some(new_text)
        && roundtrip.get("formula").and_then(Value::as_str) == Some("");
    let original_unchanged = read_bounded(input, INPUT_CAP)? == source;
    let removed: Vec<&String> = input_parts
        .keys()
        .filter(|name| !output_parts.contains_key(*name))
        .collect();
    let added: Vec<&String> = output_parts
        .keys()
        .filter(|name| !input_parts.contains_key(*name))
        .collect();
    let changed: Vec<&String> = input_parts
        .iter()
        .filter_map(|(name, before)| {
            output_parts
                .get(name)
                .filter(|after| *after != before)
                .map(|_| name)
        })
        .collect();
    let opaque_checks: Vec<Value> = OPAQUE_PARTS
        .iter()
        .map(|name| {
            let before = input_parts.get(*name);
            let after = output_parts.get(*name);
            json!({"part":name, "present_before":before.is_some(), "present_after":after.is_some(),
            "bytes_preserved":before.is_some() && before == after})
        })
        .collect();
    let opaque_pass = opaque_checks
        .iter()
        .all(|value| value.get("bytes_preserved").and_then(Value::as_bool) == Some(true));
    Ok(json!({
        "engine":"umya-spreadsheet", "expected_version":"3.1.1",
        "expected_source_commit":"88279e868caf1f02fb7fd5340eed3865ce138f31",
        "mode":if lazy { "lazy_with_target_sheet_materialized" } else { "eager" },
        "input":input, "output":output, "input_sha256":hash(&source), "output_sha256":hash(&emitted),
        "original_unchanged":original_unchanged,
        "edit":{"sheet":sheet_name,"cell":cell,"replacement_type":"string","before":before,
            "after_edit":after_edit,"readback":roundtrip,"edit_round_trip_passed":edited_value_matches},
        "package_delta":{"removed":removed,"added":added,"changed":changed,
            "input_part_count":input_parts.len(),"output_part_count":output_parts.len()},
        "opaque_part_checks":opaque_checks,"opaque_parts_preserved":opaque_pass,
        "candidate_passed_this_fixture":original_unchanged && edited_value_matches && opaque_pass,
        "limitations":[
            "same-engine readback is not an independent Excel compatibility check",
            "part bytes alone do not establish relationships/extensions/semantic consistency",
            "one string edit does not verify every advanced object or format",
            "no formula calculation, external refresh or macro execution was requested",
            "admission/output caps are not total RSS/wall-time or hostile-XML guarantees",
            "output-budget check follows writer buffering; no hard memory ceiling is claimed",
            "writer failure may leave only the newly created partial output for investigation"
        ]
    }))
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() != 6
        || !matches!(arguments.last().map(String::as_str), Some("eager" | "lazy"))
    {
        eprintln!(
            "usage: umya-roundtrip-probe INPUT.xlsx NEW_OUTPUT.xlsx SHEET CELL TEXT eager|lazy"
        );
        std::process::exit(2);
    }
    let result = run(
        Path::new(&arguments[0]),
        Path::new(&arguments[1]),
        &arguments[2],
        &arguments[3],
        &arguments[4],
        arguments[5] == "lazy",
    );
    match result {
        Ok(report) => {
            println!("{report:#}");
            // Retain candidate output and JSON even if preservation checks fail.
            if report
                .get("candidate_passed_this_fixture")
                .and_then(Value::as_bool)
                != Some(true)
            {
                std::process::exit(1);
            }
        }
        Err(error) => {
            println!(
                "{}",
                json!({"probe_error":error.to_string(),"candidate_passed_this_fixture":false})
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_coordinates_are_bounded_and_column_row_order_is_known() {
        assert_eq!(coordinate("B2").expect("valid cell"), (2, 2));
        assert_eq!(
            coordinate("XFD1048576").expect("last Excel cell"),
            (16384, 1048576)
        );
        for bad in [
            "",
            "A0",
            "A",
            "1",
            "A1:B2",
            "$A$1",
            "a1",
            "XFE1",
            "A1048577",
            "A999999999999999999999999",
        ] {
            assert!(coordinate(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn xlsx_codec_runs_without_excel_and_keeps_literal_and_formula_distinct() {
        let mut book = umya_spreadsheet::new_file();
        let sheet = book.sheet_by_name_mut("Sheet1").expect("default worksheet");
        sheet.cell_mut((1, 1)).set_value_string("=SUM(1,2)");
        sheet.cell_mut((2, 1)).set_formula("SUM(1,2)");
        let mut bytes = Vec::new();
        umya_spreadsheet::writer::xlsx::write_writer(&book, &mut bytes).expect("headless write");
        let read = umya_spreadsheet::reader::xlsx::read_reader(Cursor::new(bytes), true)
            .expect("headless read");
        let sheet = read.sheet_by_name("Sheet1").expect("readback worksheet");
        let literal = sheet.cell((1, 1)).expect("literal");
        assert_eq!(literal.value().as_ref(), "=SUM(1,2)");
        assert_eq!(literal.formula(), "");
        assert_eq!(sheet.cell((2, 1)).expect("formula").formula(), "SUM(1,2)");
        // Formula text retention does not establish evaluation or cache freshness.
    }
}
