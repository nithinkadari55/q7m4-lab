//! Core-only synthetic evaluator diagnostic. Not compiled or adopted locally.
//! Run each case in its own bounded process through run_probe.py. No XLSX IO,
//! cache injection, formula rewriting, external sources or production data.

use formualizer_common::{DateSystem, ExcelError, ExcelErrorExtra, ExcelErrorKind, LiteralValue};
use formualizer_eval::engine::named_range::{NameScope, NamedDefinition};
use formualizer_eval::engine::{Engine, EvalConfig};
use formualizer_eval::traits::{
    EvaluationContext, FunctionProvider, InMemoryRange, NamedRangeResolver, Range, RangeResolver,
    ReferenceResolver, Resolver, SourceResolver, Table, TableResolver,
};
use formualizer_parse::parser::{TableReference, parse};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FIXTURE: &str = include_str!("model-fixture.json");
const FIXTURE_HASH: &str = "ff3e0352c6ab5be142448df56a085319323d9cf6479d53455128885423fdae9f";
const COMMIT: &str = "dfd2f96457ff0929114a3e6b786c2761e6db5c5d";

#[derive(Deserialize)]
struct Fixture {
    source_commit: String,
    literals: Vec<Input>,
    names: BTreeMap<String, String>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Input {
    sheet: String,
    address: String,
    value: Value,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    corpus: String,
    sheet: String,
    address: String,
    formula: String,
    expected: Value,
    required_names: Vec<String>,
}

// A bounded mirror serves the legacy range callback, which Engine delegates to
// its external resolver. Only actual literals and a freshly calculated precursor
// enter it. This is a diagnostic closure model, never a general file loader.
type Cells = Arc<Mutex<BTreeMap<(String, u32, u32), LiteralValue>>>;
struct CoreContext { cells: Cells }
impl ReferenceResolver for CoreContext {
    fn resolve_cell_reference(&self, sheet: Option<&str>, row: u32, col: u32) -> Result<LiteralValue, ExcelError> {
        let sheet = sheet.filter(|s| matches!(*s, "Inputs" | "Calc"))
            .ok_or_else(|| ExcelError::new(ExcelErrorKind::Ref))?;
        if row == 0 || row > 1_048_576 || col == 0 || col > 16_384 {
            return Err(ExcelError::new(ExcelErrorKind::Ref));
        }
        let cells = self.cells.lock().map_err(|_| ExcelError::new(ExcelErrorKind::Error))?;
        Ok(cells.get(&(sheet.to_owned(), row, col)).cloned().unwrap_or(LiteralValue::Empty))
    }
}
impl RangeResolver for CoreContext {
    fn resolve_range_reference(&self, sheet: Option<&str>, sr: Option<u32>, sc: Option<u32>, er: Option<u32>, ec: Option<u32>) -> Result<Box<dyn Range>, ExcelError> {
        let sheet = sheet.filter(|s| matches!(*s, "Inputs" | "Calc"))
            .ok_or_else(|| ExcelError::new(ExcelErrorKind::Ref))?;
        let cells = self.cells.lock().map_err(|_| ExcelError::new(ExcelErrorKind::Error))?;
        // Only the existing SUM(Inputs!A:A) sparse-reduction control uses open
        // bounds. Trimming blank tails is NOT a ROWS/shape-preserving contract.
        let used_row = cells.keys().filter(|(s, _, _)| s == sheet).map(|(_, r, _)| *r).max().unwrap_or(1);
        let used_col = cells.keys().filter(|(s, _, _)| s == sheet).map(|(_, _, c)| *c).max().unwrap_or(1);
        let (sr, sc, er, ec) = (sr.unwrap_or(1), sc.unwrap_or(1), er.unwrap_or(used_row), ec.unwrap_or(used_col));
        if sr == 0 || sc == 0 || er < sr || ec < sc || er > 1_048_576 || ec > 16_384 {
            return Err(ExcelError::new(ExcelErrorKind::Ref));
        }
        if u64::from(er - sr + 1) * u64::from(ec - sc + 1) > 10_000 {
            return Err(ExcelError::new(ExcelErrorKind::Num));
        }
        let rows = (sr..=er).map(|r| (sc..=ec).map(|c| {
            cells.get(&(sheet.to_owned(), r, c)).cloned().unwrap_or(LiteralValue::Empty)
        }).collect()).collect();
        Ok(Box::new(InMemoryRange::new(rows)))
    }
}
impl NamedRangeResolver for CoreContext {
    fn resolve_named_range_reference(&self, _: &str) -> Result<Vec<Vec<LiteralValue>>, ExcelError> {
        Err(ExcelError::new(ExcelErrorKind::Name))
    }
}
impl TableResolver for CoreContext {
    fn resolve_table_reference(&self, _: &TableReference) -> Result<Box<dyn Table>, ExcelError> {
        Err(ExcelError::new(ExcelErrorKind::Ref))
    }
}
impl FunctionProvider for CoreContext {
    fn get_function(&self, ns: &str, name: &str) -> Option<Arc<dyn formualizer_eval::function::Function>> {
        formualizer_eval::function_registry::get(ns, name)
    }
}
impl SourceResolver for CoreContext {}
impl Resolver for CoreContext {}
impl EvaluationContext for CoreContext {}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hashed_json(value: &Value) -> String {
    hash(value.to_string().as_bytes())
}

fn failure(case: &Case, stage: &str, details_hash: String) -> Value {
    json!({"id": case.id, "status": "probe_error", "class": stage, "hash": details_hash})
}

fn error_record(case: &Case, status: &str, error: ExcelError) -> Value {
    if let ExcelErrorExtra::Resource { detail } = error.extra() {
        return json!({"id": case.id, "status": "resource_limit", "class": format!("resource:{}", detail.reason.as_str()), "hash": hash(detail.reason.as_str().as_bytes())});
    }
    json!({"id": case.id, "status": status, "class": format!("error:{}", error.kind), "hash": hash(error.kind.to_string().as_bytes())})
}

fn addr(text: &str) -> Option<(u32, u32)> {
    let mut col = 0u32;
    let mut offset = 0usize;
    for ch in text.bytes().take_while(u8::is_ascii_uppercase) {
        col = col.checked_mul(26)?.checked_add(u32::from(ch - b'A' + 1))?;
        offset += 1;
    }
    let row = text.get(offset..)?.parse::<u32>().ok()?;
    (row > 0 && row <= 1_048_576 && col > 0 && col <= 16_384).then_some((row, col))
}

fn literal(value: &Value) -> Option<LiteralValue> {
    match value {
        Value::String(value) => Some(LiteralValue::Text(value.clone())),
        Value::Number(value) => value.as_f64().filter(|n| n.is_finite()).map(LiteralValue::Number),
        Value::Bool(value) => Some(LiteralValue::Boolean(*value)),
        _ => None,
    }
}

fn setup(fixture: &Fixture, case: &Case) -> Result<Engine<CoreContext>, (&'static str, String)> {
    let mut config = EvalConfig {
        enable_parallel: false,
        max_threads: Some(1),
        date_system: DateSystem::Excel1900,
        ..EvalConfig::default()
    };
    // Logical accounting limits, not a guaranteed process-RSS sandbox. The
    // parent diagnostic also applies a per-case wall-clock/output guard.
    config.evaluation_budgets.admission.graph_vertex_hard_limit = Some(2_000);
    config.evaluation_budgets.admission.graph_edge_hard_limit = Some(8_000);
    config.evaluation_budgets.retained.total_bytes = Some(64 * 1024 * 1024);
    config.evaluation_budgets.scratch.total_bytes = Some(64 * 1024 * 1024);
    config.evaluation_budgets.scratch.materialized_array_cells = Some(10_000);
    config.evaluation_budgets.work.max_work_units = Some(1_000_000);
    config.evaluation_budgets.deadline.max_elapsed = Some(Duration::from_secs(3));
    config.spill.max_spill_cells = 10_000;
    // Keep Native temporal egress: Date/DateTime observations must be visible.
    let cells: Cells = Arc::new(Mutex::new(BTreeMap::new()));
    let mut engine = Engine::new(CoreContext { cells: Arc::clone(&cells) }, config);
    for sheet in ["Inputs", "Calc"] {
        engine.add_sheet(sheet).map_err(|err| ("sheet_setup", hash(err.kind.to_string().as_bytes())))?;
    }
    for input in &fixture.literals {
        let (row, col) = addr(&input.address).ok_or(("fixture_address", hash(b"invalid")))?;
        let value = literal(&input.value).ok_or(("fixture_literal", hash(b"invalid")))?;
        engine.set_cell_value(&input.sheet, row, col, value.clone())
            .map_err(|err| ("literal_setup", hash(err.kind.to_string().as_bytes())))?;
        cells.lock().map_err(|_| ("literal_setup", hash(b"mirror_lock")))?
            .insert((input.sheet.clone(), row, col), value);
    }
    for name in &case.required_names {
        let formula = fixture.names.get(name).ok_or(("fixture_name", hash(b"missing")))?;
        let ast = parse(formula).map_err(|err| ("name_parse", hash(err.to_string().as_bytes())))?;
        engine.define_name(name, NamedDefinition::Formula { ast, dependencies: Vec::new(), range_deps: Vec::new() }, NameScope::Workbook)
            .map_err(|err| ("name_registration", hash(err.kind.to_string().as_bytes())))?;
    }
    // The only formula precedent required by these corpora. This is calculated,
    // never populated with the native expectation or the workbook's old cache.
    let precedent = parse("A1*2").map_err(|err| ("precedent_parse", hash(err.to_string().as_bytes())))?;
    engine.set_cell_formula("Inputs", 1, 2, precedent)
        .map_err(|err| ("precedent_setup", hash(err.kind.to_string().as_bytes())))?;
    let actual_precursor = engine.evaluate_cell("Inputs", 1, 2)
        .map_err(|err| ("precedent_evaluation", hash(err.kind.to_string().as_bytes())))?
        .ok_or(("precedent_unobserved", hash(b"none")))?;
    // Preserve the candidate observation, including an Error variant, in the
    // callback mirror. Never write expected_case values or replace the formula.
    cells.lock().map_err(|_| ("precedent_setup", hash(b"mirror_lock")))?
        .insert(("Inputs".to_owned(), 1, 2), actual_precursor);
    Ok(engine)
}

fn observe(case: &Case, value: LiteralValue) -> Value {
    let (class, raw, comparable, temporal) = match value {
        LiteralValue::Int(value) => ("Int", json!({"variant": "Int", "value": value}), Some(json!(value)), false),
        LiteralValue::Number(value) if value.is_finite() => ("Number", json!({"variant": "Number", "value": value}), Some(json!(value)), false),
        LiteralValue::Number(_) => ("NonFinite", json!({"variant": "NonFinite"}), None, false),
        LiteralValue::Text(value) => ("Text", json!({"variant": "Text", "value": value.clone()}), Some(json!(value)), false),
        LiteralValue::Boolean(value) => ("Boolean", json!({"variant": "Boolean", "value": value}), Some(json!(value)), false),
        LiteralValue::Date(value) => ("Date", json!({"variant": "Date", "value": value.to_string()}), Some(json!(formualizer_common::date_to_serial_for(DateSystem::Excel1900, &value))), true),
        LiteralValue::DateTime(value) => ("DateTime", json!({"variant": "DateTime", "value": value.to_string()}), Some(json!(formualizer_common::datetime_to_serial_for(DateSystem::Excel1900, &value))), true),
        LiteralValue::Error(err) => return error_record(case, "mismatch", err),
        LiteralValue::Array(rows) => ("Array", json!({"variant": "Array", "payload_hash": hash(format!("{rows:?}").as_bytes())}), None, false),
        LiteralValue::Empty => ("Empty", json!({"variant": "Empty"}), None, false),
        LiteralValue::Pending => ("Pending", json!({"variant": "Pending"}), None, false),
        other => ("OtherTemporal", json!({"variant": "OtherTemporal", "payload_hash": hash(format!("{other:?}").as_bytes())}), None, true),
    };
    let same = match comparable.as_ref() {
        Some(observed) if observed.is_number() && case.expected.is_number() => observed.as_f64() == case.expected.as_f64(),
        Some(observed) => *observed == case.expected,
        None => false,
    };
    let status = if temporal { "representation_unqualified" } else if same { "match" } else { "mismatch" };
    let mut record = json!({"id": case.id, "status": status, "class": class, "hash": hashed_json(&raw)});
    if temporal {
        record["comparison_class"] = json!("candidate_excel1900_calendar_serial_not_independently_qualified");
        record["comparison_status"] = json!(if comparable.is_none() { "unavailable" } else if same { "candidate_match" } else { "candidate_mismatch" });
        record["comparison_hash"] = comparable.as_ref().map(hashed_json).map(Value::String).unwrap_or(Value::Null);
    }
    record
}

fn run_case(fixture: &Fixture, case: &Case) -> Value {
    let mut engine = match setup(fixture, case) {
        Ok(engine) => engine,
        Err((stage, digest)) => return failure(case, stage, digest),
    };
    let Some((row, col)) = addr(&case.address) else { return failure(case, "fixture_address", hash(b"invalid")); };
    let ast = match parse(&case.formula) {
        Ok(ast) => ast,
        Err(err) => return failure(case, "target_parse", hash(err.to_string().as_bytes())),
    };
    if let Err(err) = engine.set_cell_formula(&case.sheet, row, col, ast) {
        return failure(case, "target_ingest", hash(err.kind.to_string().as_bytes()));
    }
    match engine.evaluate_cell(&case.sheet, row, col) {
        Ok(Some(value)) => observe(case, value),
        Ok(None) => failure(case, "unobserved", hash(b"none")),
        Err(err) => error_record(case, "evaluation_error", err),
    }
    // Engine is dropped naturally at the process boundary. No close() exists
    // or is needed on this native owned Engine; panic/abort/timeout are separate.
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let fixture: Fixture = match serde_json::from_str::<Fixture>(FIXTURE) {
        Ok(fixture) if hash(FIXTURE.as_bytes()) == FIXTURE_HASH && fixture.source_commit == COMMIT && fixture.cases.len() == 53 => fixture,
        _ => { println!("{{\"status\":\"probe_error\",\"class\":\"fixture_admission\"}}"); std::process::exit(2); }
    };
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--catalog") {
        let cases: Vec<Value> = fixture.cases.iter().enumerate().map(|(index, case)| json!({"index": index, "id": case.id, "corpus": case.corpus})).collect();
        println!("{}", json!({"class": "core_only_no_xlsx_import", "source_commit": COMMIT, "fixture_hash": FIXTURE_HASH, "cases": cases}));
        return;
    }
    let case = if args.get(1).map(String::as_str) == Some("--case") {
        args.get(2).and_then(|index| index.parse::<usize>().ok()).and_then(|index| fixture.cases.get(index))
    } else { None };
    let Some(case) = case else { println!("{{\"status\":\"probe_error\",\"class\":\"arguments\"}}"); std::process::exit(2); };
    let record = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_case(&fixture, case)))
        .unwrap_or_else(|_| failure(case, "panic", hash(b"panic_payload_suppressed")));
    println!("{record}");
}
