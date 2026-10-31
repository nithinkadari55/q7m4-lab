//! Source-pinned workbook-loader diagnostic over hash-admitted authored files.
//! No formula/name injection, source edits, save/export or production backend.
use formualizer_common::{DateSystem, LiteralValue};
use formualizer_eval::engine::{FormulaSpoolDiskPolicy, TemporalEgress};
use formualizer_workbook::{IoError, LoadStrategy, NativeXlsxAdapter, Workbook, WorkbookConfig, XlsxLoadLimits};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const MANIFEST: &str = include_str!("import-fixtures.json");
const MANIFEST_HASH: &str = "87fd87d331ac506c2f178a50b8ed8827860302bbee4aa3887e10c965dfc0751b";
const COMMIT: &str = "dfd2f96457ff0929114a3e6b786c2761e6db5c5d";
static PRIVATE_DIAGNOSTIC: AtomicBool = AtomicBool::new(false);

#[derive(Deserialize)]
struct Manifest { source_commit: String, files: Vec<InputFile>, slots: Vec<Slot> }
#[derive(Deserialize)]
struct InputFile { id: String, filename: String, sha256: String, date_system: String }
#[derive(Deserialize)]
struct Slot {
    id: String, file_id: String, policy: String, binding: String,
    sheet: String, row: u32, col: u32, expected: Expected,
    expected_calendar: Option<String>,
}
#[derive(Deserialize)]
struct Expected { kind: String, value: Value }

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}
fn hashed(value: &Value) -> String { hash(value.to_string().as_bytes()) }
fn detail(error: impl std::fmt::Display) -> String {
    let message = error.to_string();
    if PRIVATE_DIAGNOSTIC.load(Ordering::Relaxed) {
        eprintln!("{}", message.chars().take(2048).collect::<String>());
    }
    hash(message.as_bytes())
}

fn raw(value: &LiteralValue) -> (String, Value) {
    let (class, payload) = match value {
        LiteralValue::Int(v) => ("Int", json!(v)),
        LiteralValue::Number(v) if v.is_finite() => ("Number", json!(v)),
        LiteralValue::Number(_) => ("NonFinite", Value::Null),
        LiteralValue::Text(v) => ("Text", json!(v)),
        LiteralValue::Boolean(v) => ("Boolean", json!(v)),
        LiteralValue::Date(v) => ("Date", json!(v.to_string())),
        LiteralValue::DateTime(v) => ("DateTime", json!(v.format("%Y-%m-%dT%H:%M:%S%.f").to_string())),
        LiteralValue::Time(v) => ("Time", json!(v.to_string())),
        LiteralValue::Duration(v) => ("Duration", json!(v.to_string())),
        LiteralValue::Array(v) => ("Array", json!(format!("{v:?}"))),
        LiteralValue::Empty => ("Empty", Value::Null),
        LiteralValue::Pending => ("Pending", Value::Null),
        LiteralValue::Error(v) => return (format!("error:{}", v.kind), json!({"variant": "Error", "kind": v.kind.to_string(), "detail_hash": detail(v)})),
    };
    (class.into(), json!({"variant": class, "value": payload}))
}

fn observe(slot: &Slot, actual: &LiteralValue) -> (&'static str, String, String) {
    let (class, payload) = raw(actual);
    if PRIVATE_DIAGNOSTIC.load(Ordering::Relaxed) {
        eprintln!("private_result={}", payload.to_string().chars().take(2048).collect::<String>());
    }
    let expected = &slot.expected;
    let numeric = match actual {
        LiteralValue::Number(value) if value.is_finite() => Some(*value),
        LiteralValue::Int(value) => Some(*value as f64),
        _ => None,
    };
    if matches!(actual, LiteralValue::Date(_) | LiteralValue::DateTime(_)) {
        if slot.policy == "native" {
            if let Some(calendar) = &slot.expected_calendar {
                let actual_calendar = match actual {
                    LiteralValue::Date(value) => format!("{value}T00:00:00"),
                    LiteralValue::DateTime(value) => value.format("%Y-%m-%dT%H:%M:%S%.f").to_string(),
                    _ => unreachable!(),
                };
                return (if actual_calendar == *calendar { "calendar_match" } else { "calendar_mismatch" }, class, hashed(&payload));
            }
        }
        return ("representation_unqualified", class, hashed(&payload));
    }
    if matches!(actual, LiteralValue::Time(_) | LiteralValue::Duration(_)) {
        return ("representation_unqualified", class, hashed(&payload));
    }
    let same = match expected.kind.as_str() {
        // Explicit tolerance for transport of fractional-day values. A bool is
        // never admitted as a numeric result; this is not a general numeric SLO.
        "number" => numeric.zip(expected.value.as_f64()).is_some_and(|(a, b)| (a - b).abs() <= 1e-10),
        "text" => matches!(actual, LiteralValue::Text(value) if expected.value.as_str() == Some(value.as_str())),
        "boolean" => matches!(actual, LiteralValue::Boolean(value) if expected.value.as_bool() == Some(*value)),
        "error" => matches!(actual, LiteralValue::Error(value) if expected.value.as_str() == Some(value.kind.to_string().as_str())),
        "empty" => matches!(actual, LiteralValue::Empty),
        _ => false,
    };
    (if same { "match" } else { "mismatch" }, class, hashed(&payload))
}

fn base(slot: &Slot, file: &InputFile) -> Value {
    json!({"id": slot.id, "status": "probe_error", "class": "unobserved", "hash": hash(b"not observed"),
        "source_sha256": file.sha256, "effective_epoch": null,
        "name_diagnostic_count": null, "name_diagnostic_hash": null,
        "cache_class": null, "cache_hash": null})
}
fn fail(mut record: Value, status: &str, stage: &str, error: impl std::fmt::Display) -> Value {
    record["status"] = json!(status); record["class"] = json!(stage); record["hash"] = json!(detail(error)); record
}
fn config(slot: &Slot, file: &InputFile) -> Result<WorkbookConfig, &'static str> {
    let mut config = WorkbookConfig::ephemeral();
    config.eval.enable_parallel = false;
    config.eval.max_threads = Some(1);
    config.eval.date_system = match slot.binding.as_str() {
        "loader_default" => DateSystem::Excel1900,
        "explicit_source_epoch" => match file.date_system.as_str() {
            "1900" => DateSystem::Excel1900, "1904" => DateSystem::Excel1904, _ => return Err("source epoch"),
        },
        _ => return Err("binding"),
    };
    config.eval.temporal_egress = match slot.policy.as_str() {
        "native" => TemporalEgress::Native, "serial" => TemporalEgress::Serial, _ => return Err("policy"),
    };
    config.eval.evaluation_budgets.admission.graph_vertex_hard_limit = Some(10_000);
    config.eval.evaluation_budgets.admission.graph_edge_hard_limit = Some(50_000);
    config.eval.evaluation_budgets.retained.total_bytes = Some(64 * 1024 * 1024);
    config.eval.evaluation_budgets.scratch.total_bytes = Some(64 * 1024 * 1024);
    config.eval.evaluation_budgets.scratch.materialized_array_cells = Some(10_000);
    config.eval.evaluation_budgets.work.max_work_units = Some(1_000_000);
    config.eval.evaluation_budgets.deadline.max_elapsed = Some(Duration::from_secs(3));
    config.eval.spill.max_spill_cells = 10_000;
    config.ingest_limits.max_sheet_rows = 256;
    config.ingest_limits.max_sheet_cols = 64;
    config.ingest_limits.max_sheet_logical_cells = 20_000;
    config.ingest_limits.max_formula_spool_bytes_per_sheet = 512 * 1024;
    config.ingest_limits.max_formula_spool_bytes_per_workbook = 1024 * 1024;
    config.ingest_limits.formula_spool_memory_prefix_bytes = 512 * 1024;
    config.ingest_limits.max_formula_spool_memory_bytes = 1024 * 1024;
    config.ingest_limits.max_formula_spool_files_per_workbook = 0;
    config.ingest_limits.formula_spool_disk_policy = FormulaSpoolDiskPolicy::MemoryOnly;
    Ok(config)
}
fn bounded_read(path: &Path) -> Result<Vec<u8>, std::io::Error> {
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > 1024 * 1024 {
        return Err(std::io::Error::other("fixture size/type"));
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 { return Err(std::io::Error::other("fixture size changed")); }
    Ok(bytes)
}
fn run(slot: &Slot, file: &InputFile, directory: &Path) -> Value {
    let record = base(slot, file);
    if Path::new(&file.filename).file_name().and_then(|v| v.to_str()) != Some(file.filename.as_str()) || !file.filename.ends_with(".xlsx") {
        return fail(record, "probe_error", "fixture_path", "basename required");
    }
    let path = directory.join(&file.filename);
    let bytes = match bounded_read(&path) { Ok(bytes) => bytes, Err(error) => return fail(record, "probe_error", "fixture_read", error) };
    if bytes.len() > 1024 * 1024 || hash(&bytes) != file.sha256 {
        return fail(record, "probe_error", "fixture_admission", "source byte identity/size");
    }
    let config = match config(slot, file) { Ok(config) => config, Err(error) => return fail(record, "probe_error", "configuration", error) };
    let limits = XlsxLoadLimits { max_input_bytes: 1024 * 1024, max_entries: 64, max_expanded_bytes: 2 * 1024 * 1024,
        max_part_bytes: 512 * 1024, max_xml_depth: 128 };
    let adapter = match NativeXlsxAdapter::open_bytes_with_limits(bytes, limits) {
        Ok(adapter) => adapter, Err(error) => return fail(record, "load_error", "source_admission", error),
    };
    let mut book = match Workbook::from_reader(adapter, LoadStrategy::EagerAll, config) {
        Ok(book) => book, Err(error) => return fail(record, "load_error", "workbook_ingest", error),
    };
    let mut record = record;
    record["effective_epoch"] = json!(book.eval_config().date_system.to_string());
    let diagnostics = book.name_import_diagnostics();
    record["name_diagnostic_count"] = json!(diagnostics.len());
    record["name_diagnostic_hash"] = json!(hash(format!("{diagnostics:?}").as_bytes()));
    let cached = book.get_value(&slot.sheet, slot.row, slot.col).unwrap_or(LiteralValue::Empty);
    let (cache_class, cache) = raw(&cached);
    record["cache_class"] = json!(cache_class); record["cache_hash"] = json!(hashed(&cache));
    match book.evaluate_cell(&slot.sheet, slot.row, slot.col) {
        Ok(actual) => {
            let (status, class, digest) = observe(slot, &actual);
            record["status"] = json!(status); record["class"] = json!(class); record["hash"] = json!(digest);
        }
        Err(error) => {
            let class = match &error { IoError::Engine(error) => format!("error:{}", error.kind), _ => "workbook_evaluation".into() };
            record = fail(record, "evaluation_error", &class, error);
        }
    }
    if !bounded_read(&path).is_ok_and(|bytes| hash(&bytes) == file.sha256) {
        return fail(record, "probe_error", "source_changed", "source must remain immutable");
    }
    record
}
fn manifest() -> Result<Manifest, &'static str> {
    if hash(MANIFEST.as_bytes()) != MANIFEST_HASH { return Err("manifest digest"); }
    let manifest: Manifest = serde_json::from_str(MANIFEST).map_err(|_| "manifest shape")?;
    if manifest.source_commit != COMMIT || manifest.files.is_empty() || manifest.slots.is_empty() { return Err("manifest identity"); }
    Ok(manifest)
}
fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let args: Vec<String> = std::env::args().collect();
    PRIVATE_DIAGNOSTIC.store(args.iter().any(|arg| arg == "--private-diagnostic"), Ordering::Relaxed);
    let manifest = match manifest() { Ok(manifest) => manifest, Err(_) => { println!("{{\"class\":\"manifest_admission\",\"status\":\"probe_error\"}}"); std::process::exit(2); } };
    if args.get(1).map(String::as_str) == Some("--catalog") {
        println!("{}", json!({"class":"actual_xlsx_import", "source_commit":COMMIT, "manifest_sha256":MANIFEST_HASH,
            "slots":manifest.slots.iter().enumerate().map(|(index, slot)| json!({"index":index,"id":slot.id})).collect::<Vec<_>>()}));
        return;
    }
    let index = args.get(2).and_then(|v| v.parse::<usize>().ok());
    let directory = args.get(3).map(Path::new);
    let Some((slot, file, directory)) = index.and_then(|i| manifest.slots.get(i)).and_then(|slot|
        manifest.files.iter().find(|file| file.id == slot.file_id).zip(directory).map(|(file, dir)| (slot, file, dir))) else {
        println!("{{\"class\":\"arguments\",\"status\":\"probe_error\"}}"); std::process::exit(2);
    };
    if args.get(1).map(String::as_str) != Some("--case") { std::process::exit(2); }
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(slot, file, directory)))
        .unwrap_or_else(|_| fail(base(slot, file), "probe_error", "panic", "panic payload suppressed"));
    println!("{observed}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use formualizer_common::ExcelErrorKind;
    #[test]
    fn digest_vectors_and_manifest_identity() {
        assert_eq!(hash(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hash(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(manifest().is_ok());
    }
    fn slot(kind: &str, expected: Value) -> Slot {
        Slot { id: "calibration".into(), file_id: "calibration".into(), policy: "native".into(), binding: "loader_default".into(),
            sheet: "Cal".into(), row:1, col:2, expected:Expected { kind:kind.into(), value:expected }, expected_calendar:None }
    }
    #[test]
    fn typed_and_error_comparison_controls() {
        assert_eq!(observe(&slot("number", json!(1)), &LiteralValue::Boolean(true)).0, "mismatch");
        let error = LiteralValue::Error(formualizer_common::ExcelError::new(ExcelErrorKind::Div));
        assert_eq!(observe(&slot("error", json!("#DIV/0!")), &error).0, "match");
        assert_eq!(observe(&slot("number", json!(1)), &error).0, "mismatch");
    }
    #[test]
    fn calendar_binding_and_unqualified_controls() {
        let actual = LiteralValue::DateTime("2024-02-29T12:34:56".parse().unwrap());
        let mut target = slot("number", json!(45351.52425925926));
        assert_eq!(observe(&target, &actual).0, "representation_unqualified");
        target.expected_calendar = Some("2024-02-29T12:34:56".into());
        assert_eq!(observe(&target, &actual).0, "calendar_match");
        target.expected_calendar = Some("2020-02-28T12:34:56".into());
        assert_eq!(observe(&target, &actual).0, "calendar_mismatch");
    }
    #[test]
    fn workbook_builtin_calibration() {
        let mut book = Workbook::new_with_config(WorkbookConfig::ephemeral());
        book.add_sheet("Cal").unwrap();
        book.set_value("Cal", 1, 1, LiteralValue::Number(3.0)).unwrap();
        book.set_formula("Cal", 1, 2, "=SUM(A1,2)").unwrap();
        assert_eq!(observe(&slot("number", json!(5)), &book.evaluate_cell("Cal",1,2).unwrap()).0, "match");
    }
    #[test]
    fn actual_fixture_import_calibration() {
        let manifest = manifest().unwrap();
        let target = manifest.slots.iter().find(|slot| slot.id == "control16/loader_default/serial/direct").unwrap();
        let file = manifest.files.iter().find(|file| file.id == target.file_id).unwrap();
        let result = run(target, file, &Path::new(env!("CARGO_MANIFEST_DIR")).join("fixture-inputs"));
        assert_eq!(result["status"], "match");
        assert_eq!(result["effective_epoch"], "1900");
    }
}
