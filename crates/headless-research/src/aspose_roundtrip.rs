//! Pinned Aspose FOSS synthetic-fixture diagnostic; source reviewed, not built yet.
//! No Office/Windows, external refresh, formula evaluation or macro execution.
mod aspose_guard;

use aspose_cells_foss_rust::{CellValue, LoadFormat, LoadOptions, Workbook, Worksheet};
use aspose_guard::*;
use serde_json::{Value, json};
use std::{fs::OpenOptions, io::Write, path::Path};

fn snapshot(sheet: &Worksheet, position: (u32, u32)) -> Value {
    // Generic validated coordinate is one-based (column,row); this API is
    // zero-based (row,column). Keep the conversion explicit.
    let (column, row) = position;
    match sheet.get_cell_value(row - 1, column - 1) {
        Some(CellValue::String(text)) => json!({"present":true,"kind":"string","value":text,"formula":""}),
        Some(CellValue::Formula(formula)) => json!({"present":true,"kind":"formula","formula":formula,
            "cache_debug":format!("{:?}",sheet.get_formula_cached_value(row - 1,column - 1))}),
        Some(value) => json!({"present":true,"kind":"other","value_debug":format!("{value:?}")}),
        None => json!({"present":false}),
    }
}

fn load(source: &[u8], strict_requested: bool) -> ProbeResult<Workbook> {
    // The pinned source DOES NOT forward these flags beyond checking format.
    // Explicit settings are recorded for the experiment, never trusted as
    // enforcement. Host admission guards and the external timeout still apply.
    let options = LoadOptions {
        load_format: LoadFormat::Xlsx,
        strict_mode: strict_requested,
        try_repair_package: !strict_requested,
        try_repair_xml: !strict_requested,
        preserve_unsupported_parts: true,
    };
    Ok(Workbook::load_xlsx_from_bytes_with_options(source, &options)?)
}

fn run(input: &Path, output: &Path, sheet_name: &str, cell: &str, new_text: &str, strict: bool) -> ProbeResult<Value> {
    if new_text.len() > 1024 {
        return Err(invalid("replacement text cap exceeded"));
    }
    let position = coordinate(cell)?;
    let source = read_bounded(input, INPUT_CAP)?;
    let input_parts = inventory(&source, true)?;
    let output = new_output_path(input, output)?;
    let mut book = load(&source, strict)?;
    let diagnostics_before = json!({
        "issues_debug":format!("{:?}",book.load_diagnostics().issues()),
        "has_repairs":book.load_diagnostics().has_repairs(),
        "has_data_loss_risk":book.load_diagnostics().has_data_loss_risk(),
        "warning":"empty diagnostics are not whole-file fidelity evidence; pinned loader does not populate arbitrary loss warnings",
    });
    let before = snapshot(book.worksheet(sheet_name)?, position);
    {
        let mut sheets = book.get_worksheets_mut();
        let sheet = sheets.get_by_name(sheet_name)?;
        sheet.set_cell_string(position.1 - 1, position.0 - 1, new_text);
    }
    let after_edit = snapshot(book.worksheet(sheet_name)?, position);
    // File implements Write+Seek; exclusive creation protects existing input,
    // candidate, hard-link and symlink paths. Do not use the overwriting path API.
    let mut destination = OpenOptions::new().write(true).create_new(true).open(&output)?;
    book.save_xlsx_to_stream(&mut destination)?;
    destination.flush()?;
    destination.sync_all()?;
    drop(destination);
    let emitted = read_bounded(&output, OUTPUT_CAP)?;
    let output_parts = inventory(&emitted, false)?;
    let readback = load(&emitted, strict)?;
    let roundtrip = snapshot(readback.worksheet(sheet_name)?, position);
    let edit_matches = roundtrip.get("kind").and_then(Value::as_str) == Some("string")
        && roundtrip.get("value").and_then(Value::as_str) == Some(new_text);
    let original_unchanged = read_bounded(input, INPUT_CAP)? == source;
    let removed: Vec<&String> = input_parts.keys().filter(|name| !output_parts.contains_key(*name)).collect();
    let added: Vec<&String> = output_parts.keys().filter(|name| !input_parts.contains_key(*name)).collect();
    let changed: Vec<&String> = input_parts.iter().filter_map(|(name, before)|
        output_parts.get(name).filter(|after| *after != before).map(|_| name)).collect();
    let opaque_checks: Vec<Value> = OPAQUE_PARTS.iter().map(|name| {
        let before = input_parts.get(*name);
        let after = output_parts.get(*name);
        json!({"part":name,"present_before":before.is_some(),"present_after":after.is_some(),
               "bytes_preserved":before.is_some() && before == after})
    }).collect();
    let opaque_pass = opaque_checks.iter().all(|check|
        check.get("bytes_preserved").and_then(Value::as_bool) == Some(true));
    Ok(json!({
        "engine":"Aspose.Cells FOSS Rust","expected_version":"26.7.0",
        "expected_source_commit":"1a6004af47b1ef15385f9d36d381a8172428cc7e",
        "mode":if strict {"strict_requested_not_enforced_by_pinned_loader"} else {"default_flags_requested"},
        "input":input,"output":output,"input_sha256":hash(&source),"output_sha256":hash(&emitted),
        "original_unchanged":original_unchanged,"load_diagnostics":diagnostics_before,
        "edit":{"sheet":sheet_name,"cell":cell,"replacement_type":"string","before":before,
                "after_edit":after_edit,"readback":roundtrip,"edit_round_trip_passed":edit_matches},
        "package_delta":{"removed":removed,"added":added,"changed":changed,
                         "input_part_count":input_parts.len(),"output_part_count":output_parts.len()},
        "opaque_part_checks":opaque_checks,"opaque_parts_preserved":opaque_pass,
        "candidate_passed_this_fixture":original_unchanged && edit_matches && opaque_pass,
        "limitations":[
            "same-engine readback is not an independent Excel compatibility or full fidelity check",
            "load strict/repair/preserve flags are recorded, not asserted effective in pinned source",
            "default save validation checks selected model contracts, not the entire OOXML package",
            "one string edit does not establish every advanced object, format, cache or calculation contract",
            "part-byte retention alone does not verify relationships/extensions/semantics",
            "admission caps and post-write output check do not impose a total RSS/CPU/wall-time ceiling",
            "a writer failure may leave only this newly created partial candidate for investigation"
        ]
    }))
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() != 6 || !matches!(arguments.last().map(String::as_str), Some("default" | "strict")) {
        eprintln!("usage: aspose-roundtrip-probe INPUT.xlsx NEW_OUTPUT.xlsx SHEET CELL TEXT default|strict");
        std::process::exit(2);
    }
    let result = run(Path::new(&arguments[0]),Path::new(&arguments[1]),&arguments[2],&arguments[3],
                     &arguments[4],arguments[5] == "strict");
    match result {
        Ok(report) => {
            println!("{report:#}");
            if report.get("candidate_passed_this_fixture").and_then(Value::as_bool) != Some(true) {
                std::process::exit(1);
            }
        },
        Err(error) => {
            println!("{}",json!({"probe_error":error.to_string(),"candidate_passed_this_fixture":false}));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_codec_distinguishes_literal_formula_and_explicit_cache() {
        let mut book = Workbook::new();
        {
            let mut sheets = book.get_worksheets_mut();
            let sheet = sheets.get(0).expect("default sheet");
            sheet.set_cell_string(0, 0, "=SUM(1,2)");
            sheet.set_cell_formula_with_cached_value(0, 1, "=SUM(1,2)", CellValue::Number(3.0));
        }
        let bytes = book.save_xlsx_to_bytes().expect("headless write");
        let read = load(&bytes, true).expect("headless read");
        let sheet = read.worksheet("Sheet1").expect("default sheet");
        assert_eq!(sheet.get_cell_value(0, 0), Some(&CellValue::String("=SUM(1,2)".into())));
        assert_eq!(sheet.get_cell_value(0, 1), Some(&CellValue::Formula("SUM(1,2)".into())));
        assert_eq!(sheet.get_formula_cached_value(0, 1), Some(&CellValue::Number(3.0)));
        // We supplied 3.0. This is no claim that the library calculated it.
    }

    #[test]
    fn dependent_cache_is_not_recalculated_when_a_precedent_is_written() {
        let mut book = Workbook::new();
        {
            let mut sheets = book.get_worksheets_mut();
            let sheet = sheets.get(0).expect("default sheet");
            sheet.set_cell_value(0, 0, CellValue::Number(7.0));
            sheet.set_cell_formula_with_cached_value(0, 1, "=A1*2", CellValue::Number(14.0));
            sheet.set_cell_value(0, 0, CellValue::Number(9.0));
        }
        let bytes = book.save_xlsx_to_bytes().expect("save stale-cache case");
        let read = load(&bytes, true).expect("read stale-cache case");
        let sheet = read.worksheet("Sheet1").expect("default sheet");
        assert_eq!(sheet.get_cell_value(0, 0), Some(&CellValue::Number(9.0)));
        assert_eq!(sheet.get_formula_cached_value(0, 1), Some(&CellValue::Number(14.0)));
        // Excel's mathematical result would be 18. The expected 14 records the
        // candidate's stale-cache boundary; callers must not mark this fresh.
    }
}
