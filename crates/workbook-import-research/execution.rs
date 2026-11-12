//! Finite source-pinned execution diagnostic. No workbook files or production service.
use formualizer_common::{CellAddress, ExcelError, ExcelErrorExtra, ExcelErrorKind, LiteralValue, RangeAddress};
use formualizer_eval::engine::{CancelToken, EvaluationBudgets, EvaluationTarget, TargetEvalOptions};
use formualizer_workbook::{CustomFnOptions, IoError, NamedRangeScope, Workbook, WorkbookConfig};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::time::{Duration, Instant};

const FIXTURE: &str = include_str!("execution-fixtures.json");
const FIXTURE_HASH: &str = "0f28ec5c0fbf86a0545afd523200b2e4864841f3e159371177c02f43ba51cdd7";
const COMMIT: &str = "dfd2f96457ff0929114a3e6b786c2761e6db5c5d";
static PRIVATE: AtomicBool = AtomicBool::new(false);
fn hash(bytes: &[u8]) -> String { Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect() }
fn digest(value: &Value) -> String { hash(value.to_string().as_bytes()) }
fn text<'a>(value: &'a Value, key: &str) -> &'a str { value[key].as_str().unwrap() }
fn flag(value: &Value, key: &str) -> bool { value[key].as_bool().unwrap_or(false) }
fn number(value: &Value, key: &str) -> u32 { u32::try_from(value[key].as_u64().unwrap()).unwrap() }
fn diagnostic(message: impl std::fmt::Display) -> String {
    let message = message.to_string();
    if PRIVATE.load(Ordering::Relaxed) { eprintln!("{}", message.chars().take(2048).collect::<String>()); }
    hash(message.as_bytes())
}
fn payload(actual: &LiteralValue) -> (String, Value) {
    match actual {
        LiteralValue::Number(n) if n.is_finite() => ("Number".into(), json!({"type":"Number","value":n})),
        LiteralValue::Int(n) => ("Int".into(), json!({"type":"Int","value":n})),
        LiteralValue::Boolean(v) => ("Boolean".into(), json!({"type":"Boolean","value":v})),
        LiteralValue::Text(v) => ("Text".into(), json!({"type":"Text","value":v})),
        LiteralValue::Empty => ("Empty".into(), json!({"type":"Empty"})),
        LiteralValue::Error(error) => (format!("cell_error:{}", error.kind), json!({"type":"Error","kind":error.kind.to_string(),"detail_hash":diagnostic(error)})),
        other => ("Other".into(), json!({"type":"Other","hash":diagnostic(format!("{other:?}"))})),
    }
}
fn same(actual: &LiteralValue, expected: &Value) -> bool {
    match expected["kind"].as_str() {
        Some("number") => {
            let actual = match actual { LiteralValue::Number(v) if v.is_finite() => Some(*v), LiteralValue::Int(v) => Some(*v as f64), _ => None };
            actual.zip(expected["value"].as_f64()).is_some_and(|(a,b)| (a-b).abs() <= 1e-10)
        }
        Some("text") => matches!(actual, LiteralValue::Text(v) if expected["value"].as_str() == Some(v.as_str())),
        Some("boolean") => matches!(actual, LiteralValue::Boolean(v) if expected["value"].as_bool() == Some(*v)),
        Some("error") => matches!(actual, LiteralValue::Error(v) if expected["value"].as_str() == Some(v.kind.to_string().as_str())),
        Some("empty") => matches!(actual, LiteralValue::Empty),
        _ => false,
    }
}
fn literal(kind: &str, value: &Value) -> LiteralValue {
    match kind {
        "number" => LiteralValue::Number(value.as_f64().unwrap()), "text" => LiteralValue::Text(value.as_str().unwrap().into()),
        "boolean" => LiteralValue::Boolean(value.as_bool().unwrap()), _ => panic!("authored literal type"),
    }
}
fn safe_budgets() -> EvaluationBudgets {
    let mut budget = EvaluationBudgets::default();
    budget.admission.graph_vertex_hard_limit = Some(10_000);
    budget.admission.graph_edge_hard_limit = Some(50_000);
    budget.retained.total_bytes = Some(64*1024*1024);
    budget.scratch.total_bytes = Some(64*1024*1024);
    budget.scratch.materialized_array_cells = Some(10_000);
    budget.work.max_work_units = Some(1_000_000);
    budget.deadline.max_elapsed = Some(Duration::from_secs(5));
    budget
}
fn book(deferred: bool) -> Workbook {
    let mut config = WorkbookConfig::ephemeral();
    config.eval.defer_graph_building = deferred;
    config.eval.enable_parallel = false;
    config.eval.max_threads = Some(1);
    config.eval.evaluation_budgets = safe_budgets();
    config.eval.spill.max_spill_cells = 10_000;
    Workbook::new_with_config(config)
}
fn address(value: &Value) -> RangeAddress {
    RangeAddress::new(text(value,"sheet"),number(value,"start_row"),number(value,"start_col"),number(value,"end_row"),number(value,"end_col")).unwrap()
}
fn set_formula(book: &mut Workbook, value: &Value) -> Result<(),IoError> {
    let formula = text(value,"formula");
    assert!(!formula.starts_with('='));
    book.set_formula(text(value,"sheet"),number(value,"row"),number(value,"col"),&format!("={formula}"))
}
fn cache_event(book: &Workbook, target: &Value, identity: &str, phase: &str, expected: Option<&Value>, require_current: Option<bool>) -> Value {
    let sheet = text(target,"sheet"); let row=number(target,"row"); let col=number(target,"col");
    let value = book.get_value(sheet,row,col);
    let inspected = book.engine().inspect_cell_result(&CellAddress::new(sheet,row,col).unwrap()).unwrap();
    let staleness = format!("{:?}", inspected.staleness);
    let agrees = value == inspected.value;
    let (class, raw) = payload(value.as_ref().unwrap_or(&LiteralValue::Empty));
    let comparison = expected.map(|expected| same(value.as_ref().unwrap_or(&LiteralValue::Empty),expected));
    let matches = require_current.map(|current| agrees && (staleness == "Current") == current && (!current || comparison.unwrap_or(true)));
    if PRIVATE.load(Ordering::Relaxed) { eprintln!("private_event={}", json!({"id":identity,"phase":phase,"actual":raw,"staleness":staleness})); }
    json!({"id":identity,"phase":phase,"class":class,"hash":digest(&raw),"matches":matches,
        "present":value.is_some(),"staleness":staleness,"cache_matches_expected":comparison,"inspection_agrees":agrees})
}
fn api_event(identity: &str, error: IoError) -> Value {
    json!({"id":identity,"phase":"api_error","class":"io_error","hash":diagnostic(error),"matches":false,
        "present":null,"staleness":null,"cache_matches_expected":null,"inspection_agrees":null})
}
fn telemetry(book: &Workbook) -> Value {
    book.engine().last_evaluation_resource_request_stats().map_or(Value::Null, |stats| json!({
        "request_id":stats.request_id,"kind":format!("{:?}",stats.kind),"outcome":format!("{:?}",stats.outcome),
        "work_limit":stats.ledger.work_limit,"work_charged":stats.ledger.work_charged,
        "scratch_limit":stats.ledger.scratch_limit,"scratch_peak":stats.ledger.scratch_peak,
        "retained_peak":stats.ledger.retained_peak,"exhaustion":stats.ledger.exhaustion.map(|reason|reason.as_str())
    }))
}
fn finish(case: &Value, events: Vec<Value>, preserved: Option<bool>, request: Value, retry: Value, calls: [usize;2], exercised: bool) -> Value {
    let status = if events.iter().any(|event| event["matches"] == false) || preserved == Some(false) { "difference" }
        else if !exercised { "unqualified" } else if case["expected"] == "observe" { "observed" } else { "pass" };
    let body = json!({"events":events,"config_preserved":preserved,"request_telemetry":request,"retry_telemetry":retry,"handler_calls":calls,"exercised":exercised});
    json!({"id":case["id"],"status":status,"class":case["kind"],"hash":digest(&body),"body":body})
}
fn mutation(case: &Value, manifest: &Value) -> Result<Value,IoError> {
    let model = manifest["models"].as_array().unwrap().iter().find(|model| model["id"] == case["model_id"]).unwrap();
    let mut book = book(flag(case,"deferred"));
    for sheet in model["sheets"].as_array().unwrap() { book.add_sheet(sheet.as_str().unwrap())?; }
    let cells=model["initial_cells"].as_array().unwrap();
    for cell in cells.iter().filter(|cell|cell["kind"]!="formula") {
        book.set_value(text(cell,"sheet"),number(cell,"row"),number(cell,"col"),literal(text(cell,"kind"),&cell["value"]))?;
    }
    if let Some(names)=model["initial_names"].as_array() { for name in names { book.define_named_range(text(name,"name"),&address(name),NamedRangeScope::Workbook)?; } }
    for cell in cells.iter().filter(|cell|cell["kind"]=="formula") { set_formula(&mut book,cell)?; }
    let mut events=Vec::new();
    for (index,target) in model["initial_targets"].as_array().unwrap().iter().enumerate() {
        let identity=format!("initial/{index}");
        match book.evaluate_cell(text(target,"sheet"),number(target,"row"),number(target,"col")) {
            Ok(_) => events.push(cache_event(&book,target,&identity,"baseline",Some(&target["expected"]),Some(true))),
            Err(error)=>events.push(api_event(&identity,error)),
        }
    }
    for action in model["actions"].as_array().unwrap() {
        let result=match text(action,"kind") {
            "set_value"=>book.set_value(text(action,"sheet"),number(action,"row"),number(action,"col"),literal(text(action,"value_kind"),&action["value"])),
            "set_formula"=>set_formula(&mut book,action),
            "define_named_range"=>book.update_named_range(text(action,"name"),&address(action),NamedRangeScope::Workbook),
            _=>panic!("authored action type"),
        };
        if let Err(error)=result { events.push(api_event(text(action,"id"),error)); }
        for (index,target) in action["targets"].as_array().unwrap().iter().enumerate() {
            let identity=format!("{}/{index}",text(action,"id"));
            events.push(cache_event(&book,target,&identity,"cache_before",Some(&target["expected"]),None));
            match book.evaluate_cell(text(target,"sheet"),number(target,"row"),number(target,"col")) {
                Ok(_)=>events.push(cache_event(&book,target,&identity,"after_eval",Some(&target["expected"]),Some(true))),
                Err(error)=>events.push(api_event(&identity,error)),
            }
        }
    }
    Ok(finish(case,events,None,Value::Null,telemetry(&book),[0,0],true))
}
fn io_class(error: &IoError) -> (String, Value) {
    match error {
        IoError::Engine(error)=> {
            if let ExcelErrorExtra::Resource{detail}=error.extra() {
                return (format!("resource:{}",detail.reason.as_str()),json!({"kind":error.kind.to_string(),"reason":detail.reason.as_str(),"limit":detail.limit,"observed":detail.observed,"request_id":detail.request_id,"detail_hash":diagnostic(error)}));
            }
            (if error.kind==ExcelErrorKind::Cancelled { "cancelled".into() } else { format!("api_error:{}",error.kind) },json!({"kind":error.kind.to_string(),"detail_hash":diagnostic(error)}))
        }
        _=>("io_error".into(),json!({"detail_hash":diagnostic(error)})),
    }
}
fn execution(case: &Value) -> Result<Value,IoError> {
    let mut book=book(flag(case,"deferred")); book.add_sheet("Calc")?;
    book.set_value("Calc",1,1,LiteralValue::Number(3.0))?;
    book.set_value("Calc",2,1,LiteralValue::Number(4.0))?;
    let token=CancelToken::new();
    let calls=Arc::new(AtomicUsize::new(0)); let armed=Arc::new(AtomicBool::new(true));
    let mode=case["cancel"].as_str().unwrap_or("none").to_string();
    let uses_handler=matches!(mode.as_str(),"handler-value"|"handler-error"|"error-value-only"|"deadline-handler");
    if uses_handler {
        let token=token.clone(); let calls=calls.clone(); let armed=armed.clone(); let mode=mode.clone();
        book.register_custom_function("RESEARCHGATE",CustomFnOptions{min_args:1,max_args:Some(1),deterministic:false,..Default::default()},Arc::new(move |args:&[LiteralValue]|->Result<LiteralValue,ExcelError>{
            calls.fetch_add(1,Ordering::Relaxed);
            if armed.swap(false,Ordering::Relaxed) {
                match mode.as_str() {
                    "handler-value"=>token.cancel(),
                    "handler-error"=> {token.cancel();return Err(ExcelError::new(ExcelErrorKind::Cancelled));},
                    "error-value-only"=>return Ok(LiteralValue::Error(ExcelError::new(ExcelErrorKind::Cancelled))),
                    "deadline-handler"=>std::thread::sleep(Duration::from_millis(200)),
                    _=>unreachable!(),
                }
            }
            Ok(args[0].clone())
        })).map_err(IoError::Engine)?;
    }
    book.set_formula("Calc",1,2,if flag(case,"array") {"=SUM(SEQUENCE(2,2))+A1"} else {"=SUM(A1:A2)"})?;
    book.set_formula("Calc",1,3,if uses_handler {"=RESEARCHGATE(B1)*2"} else {"=B1*2"})?;
    let target=json!({"sheet":"Calc","row":1,"col":3});
    let precedent=json!({"sheet":"Calc","row":1,"col":2});
    let expected_value=if flag(case,"array") {26.0} else {14.0};
    let expected=json!({"kind":"number","value":expected_value});
    let targets=[EvaluationTarget::Cell{sheet:"Calc".into(),row:1,col:3}];
    let route=text(case,"route");
    let plan=if route=="plan" {Some(book.build_recalc_plan_for_targets(&targets)?)} else {None};
    let mut engine=safe_budgets();
    if let Some(value)=case["engine_work"].as_u64() {engine.work.max_work_units=Some(value);}
    if let Some(value)=case["engine_deadline_ms"].as_u64() {engine.deadline.max_elapsed=Some(Duration::from_millis(value));}
    if let Some(value)=case["engine_array"].as_u64() {engine.scratch.materialized_array_cells=Some(value);}
    book.engine_mut().set_evaluation_resource_budgets(engine.clone());
    let mut request=safe_budgets();
    if let Some(value)=case["request_work"].as_u64() {request.work.max_work_units=Some(value);}
    if let Some(value)=case["request_deadline_ms"].as_u64() {request.deadline.max_elapsed=Some(Duration::from_millis(value));}
    if let Some(value)=case["request_array"].as_u64() {request.scratch.materialized_array_cells=Some(value);}
    let request_budget=["request_work","request_deadline_ms","request_array"].iter().any(|key|!case[*key].is_null());
    let mut events=vec![cache_event(&book,&target,"target","cache_before",Some(&expected),None)];
    let deadline=match case["explicit_deadline"].as_str() {Some("past")=>Some(Instant::now()-Duration::from_millis(1)),Some("during-handler")=>Some(Instant::now()+Duration::from_millis(100)),_=>None};
    if mode=="pre" {token.cancel();}
    let with_token=mode=="pre"||mode=="handler-value"||mode=="handler-error";
    let options=TargetEvalOptions{request_id:Some(77),budgets:request_budget.then_some(&request),cancel:with_token.then_some(token.clone()),deadline,..Default::default()};
    let result:Result<(),IoError>=match route {
        "cell"=>book.evaluate_cell("Calc",1,3).map(|_|()),
        "cells"=>book.evaluate_cells_cancellable(&[("Calc",1,3)],token.clone()).map(|_|()),
        "targets"=>book.evaluate_targets_with_options(&targets,options).map(|_|()),
        "all"=>book.evaluate_all_cancellable(token.clone()).map(|_|()),
        "prepare"=>book.prepare_graph_for_targets(&targets,options).map(|_|()),
        "plan"=>book.evaluate_with_plan_cancellable(plan.as_ref().unwrap(),token.clone()).map(|_|()),
        _=>panic!("authored execution route"),
    };
    let request_stats=telemetry(&book);
    let preserved=book.engine().evaluation_resource_budgets()==&engine;
    let handler_calls=calls.load(Ordering::Relaxed);
    let exercised=!uses_handler||handler_calls>0;
    let expected_outcome=text(case,"expected");
    let (class,raw,matched)=match &result {
        Ok(())=> {
            let cached=book.get_value("Calc",1,3).unwrap_or(LiteralValue::Empty);
            let (class,value)=payload(&cached);
            let matched=match expected_outcome {
                "success"=>route=="prepare"||same(&cached,&expected),
                "cell_error"=>matches!(&cached,LiteralValue::Error(error) if case["reason"].as_str()==Some(error.kind.to_string().as_str())),
                "observe"=>true,_=>false,
            };
            (if route=="prepare" {"preparation_success".into()} else {format!("success:{class}")},value,matched)
        }
        Err(error)=> {
            let (class,value)=io_class(error);
            let matched=expected_outcome=="observe" || expected_outcome=="cancelled"&&class=="cancelled" || expected_outcome=="resource"&&class==format!("resource:{}",text(case,"reason"));
            (class,value,matched)
        }
    };
    if PRIVATE.load(Ordering::Relaxed) {eprintln!("private_request={}",json!({"class":class,"payload":raw,"telemetry":request_stats}));}
    events.push(json!({"id":"request","phase":"request","class":class,"hash":digest(&raw),"matches":if expected_outcome=="observe" {None} else {Some(matched&&exercised)},
        "present":null,"staleness":null,"cache_matches_expected":null,"inspection_agrees":null}));
    let current=if expected_outcome=="observe" {None} else {Some(matches!(expected_outcome,"success"|"cell_error")&&route!="prepare")};
    let cache_expected=if expected_outcome=="cell_error" {json!({"kind":"error","value":case["reason"]})} else {expected.clone()};
    events.push(cache_event(&book,&target,"target","cache_after",Some(&cache_expected),current));
    // On cancellation/rejection, a partially computed precedent must not be
    // reported Current. The cached value may remain as a separately stale fact.
    if matches!(expected_outcome,"cancelled"|"resource") {
        events.push(cache_event(&book,&precedent,"precedent","cache_after",None,Some(false)));
    }
    book.engine_mut().set_evaluation_resource_budgets(safe_budgets());
    book.set_value("Calc",1,1,LiteralValue::Number(5.0))?;
    let retry_expected=json!({"kind":"number","value":if flag(case,"array") {30.0} else {18.0}});
    match book.evaluate_cell("Calc",1,3) {
        Ok(_)=>events.push(cache_event(&book,&target,"target","retry",Some(&retry_expected),Some(true))),
        Err(error)=>events.push(api_event("retry",error)),
    }
    let retry_stats=telemetry(&book);
    let retry_calls=calls.load(Ordering::Relaxed)-handler_calls;
    Ok(finish(case,events,Some(preserved),request_stats,retry_stats,[handler_calls,retry_calls],exercised&&(!uses_handler||retry_calls>0)))
}
fn fail(case: &Value, error: impl std::fmt::Display) -> Value {
    let body=json!({"stage":"owned_case","detail_hash":diagnostic(error)});
    json!({"id":case["id"],"status":"probe_error","class":case["kind"],"hash":digest(&body),"body":body})
}
fn manifest() -> Value {
    assert_eq!(hash(FIXTURE.as_bytes()),FIXTURE_HASH);
    let value:Value=serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(value["source_commit"],COMMIT); assert_eq!(value["cases"].as_array().unwrap().len(),41);
    value
}
fn main() {
    std::panic::set_hook(Box::new(|_|{}));
    let args:Vec<String>=std::env::args().collect(); PRIVATE.store(args.iter().any(|arg|arg=="--private-diagnostic"),Ordering::Relaxed);
    let fixture=manifest();
    if args.get(1).map(String::as_str)==Some("--catalog") {println!("{}",json!({"class":"in_memory_execution_boundaries","source_commit":COMMIT,"manifest_sha256":FIXTURE_HASH,"cases":fixture["cases"].as_array().unwrap().iter().enumerate().map(|(index,case)|json!({"index":index,"id":case["id"]})).collect::<Vec<_>>()}));return;}
    let case=if args.get(1).map(String::as_str)==Some("--case") {args.get(2).and_then(|v|v.parse::<usize>().ok()).and_then(|i|fixture["cases"].get(i))} else {None};
    let Some(case)=case else {std::process::exit(2)};
    let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| if case["kind"]=="mutation" {mutation(case,&fixture)} else {execution(case)}));
    let record=match result {Ok(Ok(record))=>record,Ok(Err(error))=>fail(case,error),Err(_)=>fail(case,"panic payload suppressed")};
    println!("{record}");
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn digest_and_frozen_fixture() {assert_eq!(hash(b"abc"),"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");assert_eq!(manifest()["models"].as_array().unwrap().len(),6);}
    #[test] fn typed_comparison_rejects_boolean_and_error_numbers() {let expected=json!({"kind":"number","value":1});assert!(!same(&LiteralValue::Boolean(true),&expected));assert!(!same(&LiteralValue::Error(ExcelError::new(ExcelErrorKind::Num)),&expected));}
    #[test] fn ordinary_workbook_integration() {let fixture=manifest();let case=&fixture["cases"][0];let result=execution(case).unwrap();assert_eq!(result["status"],"pass");}
    #[test] fn literal_cancelled_error_is_typed_data() {let error=LiteralValue::Error(ExcelError::new(ExcelErrorKind::Cancelled));assert!(same(&error,&json!({"kind":"error","value":"#CANCELLED!"})));assert!(payload(&error).0.starts_with("cell_error:"));}
}
