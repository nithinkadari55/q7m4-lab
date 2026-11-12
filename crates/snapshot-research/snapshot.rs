//! Finite source-pinned execution diagnostic. No workbook files or production service.
use formualizer_common::{CellAddress, ExcelError, ExcelErrorExtra, ExcelErrorKind, LiteralValue, RangeAddress};
use formualizer_eval::engine::{CancelToken, EvaluationBudgets, EvaluationTarget, TargetEvalOptions};
use formualizer_workbook::{CustomFnOptions, IoError, NamedRangeScope, Workbook, WorkbookConfig};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::time::{Duration, Instant};

const FIXTURE: &str = include_str!("snapshot-fixtures.json");
const FIXTURE_HASH: &str = "a02136889d55e451fa80a345a027223f555ad1a366fd83d86104af59f03ec056";
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
fn telemetry(book: &Workbook) -> Value {
    book.engine().last_evaluation_resource_request_stats().map_or(Value::Null, |stats| json!({
        "request_id":stats.request_id,"kind":format!("{:?}",stats.kind),"outcome":format!("{:?}",stats.outcome),
        "work_limit":stats.ledger.work_limit,"work_charged":stats.ledger.work_charged,
        "scratch_limit":stats.ledger.scratch_limit,"scratch_peak":stats.ledger.scratch_peak,
        "retained_peak":stats.ledger.retained_peak,"exhaustion":stats.ledger.exhaustion.map(|reason|reason.as_str())
    }))
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

use std::collections::BTreeMap;
use formualizer_common::RangeArea;
use formualizer_eval::engine::inspect::{InspectError, RangePageOptions, SnapshotOptions, StateStamp};

fn stamp(book: &Workbook) -> StateStamp {
    book.engine().inspect_cell(&CellAddress::new("_Research",1,1).unwrap(),&SnapshotOptions::default()).unwrap().stamp
}
fn stamp_value(value: StateStamp) -> Value {
    json!({"mutation_revision":value.mutation_revision,"recalc_epoch":value.recalc_epoch})
}
fn event(op: &Value, class: &str, raw: Value, details: Value, matches: bool) -> Value {
    if PRIVATE.load(Ordering::Relaxed) {eprintln!("private_event={}",json!({"id":op["id"],"kind":op["kind"],"class":class,"payload":raw,"details":details}));}
    json!({"id":op["id"],"kind":op["kind"],"class":class,"hash":digest(&raw),"details":details,"matches":matches})
}
fn apply(book: &mut Workbook, action: &Value) -> Result<(),IoError> {
    match text(action,"kind") {
        "set_value"=>book.set_value(text(action,"sheet"),number(action,"row"),number(action,"col"),literal(text(action,"value_kind"),&action["value"])),
        "set_formula"=>set_formula(book,action),
        "define_named_range"=>book.update_named_range(text(action,"name"),&address(action),NamedRangeScope::Workbook),
        _=>panic!("authored action"),
    }
}
fn read(book: &Workbook, op: &Value) -> Value {
    let target=&op["target"];
    let before=stamp(book);
    let value=book.get_value(text(target,"sheet"),number(target,"row"),number(target,"col"));
    let inspected=book.engine().inspect_cell_result(&CellAddress::new(text(target,"sheet"),number(target,"row"),number(target,"col")).unwrap()).unwrap();
    let after=stamp(book);
    let state=format!("{:?}",inspected.staleness);
    let agrees=value==inspected.value;
    let (class,raw)=payload(value.as_ref().unwrap_or(&LiteralValue::Empty));
    let expected=op.get("expected").filter(|value|!value.is_null());
    let typed=expected.map_or(true,|expected|value.as_ref().is_some_and(|actual|same(actual,expected)));
    let states=op["states"].as_array().unwrap();
    let allowed=states.is_empty()||states.iter().any(|allowed|allowed==&state);
    event(op,&class,raw,json!({"present":value.is_some(),"staleness":state,"inspection_agrees":agrees,
        "stamp":stamp_value(after),"read_only":before==after}),typed&&allowed&&agrees&&before==after)
}
fn fixture() -> Value {
    assert_eq!(hash(FIXTURE.as_bytes()),FIXTURE_HASH);
    let fixture:Value=serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(fixture["source_commit"],COMMIT);
    assert_eq!(fixture["cases"].as_array().unwrap().len(),54);
    fixture
}
fn run(case: &Value, fixture: &Value, events: &mut Vec<Value>) -> Result<(),IoError> {
    let mut book=book(flag(case,"deferred"));
    book.add_sheet("_Research")?;
    book.set_value("_Research",1,1,LiteralValue::Number(0.0))?;
    let tick=Arc::new(AtomicUsize::new(0));
    let outside=Arc::new(AtomicUsize::new(0));
    let armed=Arc::new(AtomicBool::new(false));
    let mode=Arc::new(std::sync::Mutex::new(String::new()));
    let cancel=Arc::new(std::sync::Mutex::new(CancelToken::new()));
    if case["model_id"]=="tick" {
        book.add_sheet("Calc")?;book.add_sheet("Other")?;
        book.set_value("Calc",1,1,LiteralValue::Number(3.0))?;
        let count=tick.clone();let armed=armed.clone();let mode=mode.clone();let cancel=cancel.clone();
        book.register_custom_function("RESEARCHTICK",CustomFnOptions{max_args:Some(0),volatile:true,deterministic:false,..Default::default()},
            Arc::new(move |_args:&[LiteralValue]|->Result<LiteralValue,ExcelError>{
                let value=count.fetch_add(1,Ordering::SeqCst)+1;
                if armed.swap(false,Ordering::SeqCst) {
                    let mode=mode.lock().unwrap().clone();
                    let token=cancel.lock().unwrap().clone();
                    match mode.as_str() {
                        "in_cancel"=>token.cancel(),
                        "in_error"=>{token.cancel();return Err(ExcelError::new(ExcelErrorKind::Cancelled));},
                        "contained_panic"=>panic!("owned facade callback panic"),
                        "literal_error"=>return Ok(LiteralValue::Error(ExcelError::new(ExcelErrorKind::Cancelled))),
                        _=>(),
                    }
                }
                Ok(LiteralValue::Number(value as f64))
            })).map_err(IoError::Engine)?;
        let count=outside.clone();
        book.register_custom_function("RESEARCHOUTSIDE",CustomFnOptions{max_args:Some(0),volatile:true,deterministic:false,..Default::default()},
            Arc::new(move |_args:&[LiteralValue]|->Result<LiteralValue,ExcelError>{Ok(LiteralValue::Number((count.fetch_add(1,Ordering::SeqCst)+1) as f64))})).map_err(IoError::Engine)?;
        book.set_formula("Calc",1,2,"=RESEARCHTICK()")?;
        book.set_formula("Calc",1,3,"=B1*2+A1")?;
        book.set_formula("Other",1,2,"=RESEARCHOUTSIDE()")?;
    } else {
        let model=fixture["models"].as_array().unwrap().iter().find(|model|model["id"]==case["model_id"]).unwrap();
        for sheet in model["sheets"].as_array().unwrap(){book.add_sheet(sheet.as_str().unwrap())?;}
        for cell in model["initial_cells"].as_array().unwrap().iter().filter(|cell|cell["kind"]!="formula") {
            book.set_value(text(cell,"sheet"),number(cell,"row"),number(cell,"col"),literal(text(cell,"kind"),&cell["value"]))?;
        }
        if let Some(names)=model["initial_names"].as_array(){for name in names{book.define_named_range(text(name,"name"),&address(name),NamedRangeScope::Workbook)?;}}
        for cell in model["initial_cells"].as_array().unwrap().iter().filter(|cell|cell["kind"]=="formula"){set_formula(&mut book,cell)?;}
    }
    let mut stamps:BTreeMap<String,StateStamp>=BTreeMap::new();
    let mut emitted_request_id:Option<u64>=None;
    let targets=[EvaluationTarget::Cell{sheet:"Calc".into(),row:1,col:3}];
    for op in case["ops"].as_array().unwrap(){
        let result=match text(op,"kind") {
            "read"=>read(&book,op),
            "mutate"=>{
                let result=apply(&mut book,&op["action"]);
                let (class,raw)=match result{Ok(())=>("applied".into(),json!({"applied":true})),Err(error)=>io_class(&error)};
                event(op,&class,raw,json!({"stamp":stamp_value(stamp(&book))}),class=="applied")
            }
            "counter"=>{
                let value=if op["which"]=="tick"{tick.load(Ordering::SeqCst)}else{outside.load(Ordering::SeqCst)};
                event(op,"counter",json!({"count":value}),json!({"count":value}),op["expected"].as_u64()==Some(value as u64))
            }
            "capture"=>{
                let current=stamp(&book);stamps.insert(text(op,"key").into(),current);
                event(op,"captured",stamp_value(current),json!({"stamp":stamp_value(current)}),true)
            }
            "guard"=>{
                let current=stamp(&book);let captured=stamps[text(op,"key")];
                let relation=if current==captured{"same"}else{"different"};
                let raw=json!({"captured":stamp_value(captured),"current":stamp_value(current)});
                event(op,relation,raw.clone(),raw,op["expected"]=="observe"||op["expected"]==relation)
            }
            "budget"=>{
                assert_eq!(op["mode"],"safe");book.engine_mut().set_evaluation_resource_budgets(safe_budgets());
                let safe=book.engine().evaluation_resource_budgets()==&safe_budgets();
                event(op,"configured",json!({"safe":true}),json!({"safe":safe}),safe)
            }
            "page"=>{
                let before=stamp(&book);
                let options=RangePageOptions::default().with_offset(op["offset"].as_u64().unwrap()).with_limit(number(op,"limit"))
                    .with_include_values(true).with_expected_stamp(stamps[text(op,"key")]);
                let area=RangeArea::new("Calc",Some(1),Some(1),Some(1),Some(3)).unwrap();
                let response=book.engine().range_page(&area,&options);
                let after=stamp(&book);
                match response {
                    Ok(page)=>{
                        let items:Vec<Value>=page.items.iter().map(|item|{
                            let (class,raw)=payload(item.value.as_ref().unwrap_or(&LiteralValue::Empty));
                            json!({"sheet":item.address.sheet,"row":item.address.row,"col":item.address.column,"class":class,"hash":digest(&raw),
                                "present":item.value.is_some(),"value_included":item.value_included,"staleness":format!("{:?}",item.staleness)})
                        }).collect();
                        let raw=json!({"stamp":stamp_value(page.stamp),"offset":page.offset,"total":page.total,"next_offset":page.next_offset,"items":items});
                        let expected=op["items"].as_array().unwrap();
                        let values=page.items.len()==expected.len()&&page.items.iter().zip(expected).all(|(actual,expected)|
                            actual.address.sheet=="Calc"&&actual.address.row==1&&u64::from(actual.address.column)==expected["col"].as_u64().unwrap()
                            &&actual.value_included&&format!("{:?}",actual.staleness)=="Current"&&actual.value.as_ref().is_some_and(|value|same(value,&expected["expected"])));
                        let binding=page.stamp==stamps[text(op,"key")]&&page.stamp==after;
                        let correct=op["expected"]=="page"&&before==after&&binding&&page.total==3&&page.offset==options.offset
                            &&page.next_offset==(if options.offset==0{Some(2)}else{None})&&values;
                        event(op,"page",raw.clone(),json!({"page":raw,"current_stamp":stamp_value(after),"read_only":before==after,"binding_agrees":binding}),correct)
                    }
                    Err(error)=>{
                        let class=if matches!(&error,InspectError::RevisionMismatch{..}){"revision_mismatch"}else{"inspection_error"};
                        let raw=json!({"detail_hash":diagnostic(error)});
                        let binding=class!="revision_mismatch"||after!=stamps[text(op,"key")];
                        event(op,class,raw,json!({"stamp":stamp_value(after),"read_only":before==after,"binding_agrees":binding}),op["expected"]==class&&before==after&&binding)
                    }
                }
            }
            "call"=>{
                let route=text(op,"route");let requested=text(op,"mode");
                let token=CancelToken::new();
                *cancel.lock().unwrap()=token.clone();*mode.lock().unwrap()=requested.into();
                armed.store(matches!(requested,"in_cancel"|"in_error"|"contained_panic"|"literal_error"),Ordering::SeqCst);
                if requested=="pre_cancel"{token.cancel();}
                if requested=="work_zero"{let mut budgets=safe_budgets();budgets.work.max_work_units=Some(0);book.engine_mut().set_evaluation_resource_budgets(budgets);}
                if requested=="deadline_zero"{let mut budgets=safe_budgets();budgets.deadline.max_elapsed=Some(Duration::ZERO);book.engine_mut().set_evaluation_resource_budgets(budgets);}
                let before=book.engine().evaluation_resource_budgets().clone();
                let previous_request_id=book.engine().last_evaluation_resource_request_stats().map(|stats|stats.request_id);
                let attempted=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||->Result<(),IoError>{
                    match route {
                        "snapshot"=>book.engine_mut().evaluate_all_for_snapshot(Some(token.clone())).map(|_|()).map_err(IoError::Engine),
                        "cell"=>book.evaluate_cell("Calc",1,3).map(|_|()),
                        "cells"=>book.evaluate_cells_cancellable(&[("Calc",1,3)],token.clone()).map(|_|()),
                        "targets"=>book.evaluate_targets_with_options(&targets,TargetEvalOptions{cancel:Some(token.clone()),..Default::default()}).map(|_|()),
                        "all"=>book.evaluate_all_cancellable(token.clone()).map(|_|()),
                        "prepare"=>book.prepare_graph_for_targets(&targets,TargetEvalOptions{cancel:Some(token.clone()),..Default::default()}).map(|_|()),
                        "plan"=>{let plan=book.build_recalc_plan_for_targets(&targets)?;book.evaluate_with_plan_cancellable(&plan,token.clone()).map(|_|())},
                        _=>panic!("authored route"),
                    }
                }));
                let (class,raw)=match attempted{Ok(Ok(()))=>("success".into(),json!({"kind":"success"})),Ok(Err(error))=>io_class(&error),
                    Err(_)=>("unwind".into(),json!({"kind":"unwind"}))};
                let preserved=book.engine().evaluation_resource_budgets()==&before;
                let stats=telemetry(&book);
                let fresh=stats["request_id"].as_u64().is_some_and(|id|previous_request_id.is_none_or(|previous|id>previous));
                let expected_kind=match route{"snapshot"|"all"=>"FullCancellable","cell"=>"Cell","cells"=>"CellsCancellable","targets"=>"Targeted","prepare"=>"TargetPreparation","plan"=>"RecalcPlan",_=>unreachable!()};
                let kind_agrees=stats["kind"]==expected_kind;
                let chain_agrees=emitted_request_id.is_none_or(|previous|previous_request_id==Some(previous));
                emitted_request_id=stats["request_id"].as_u64();
                let telemetry_agrees=if class=="success"{stats["outcome"]=="Success"&&stats["exhaustion"].is_null()}
                    else if class=="cancelled"{stats["outcome"]=="Cancelled"&&stats["exhaustion"].is_null()}
                    else if let Some(reason)=class.strip_prefix("resource:"){stats["outcome"]=="Error"&&stats["exhaustion"]==reason}
                    else{false};
                event(op,&class,raw,json!({"telemetry":stats,"budget_preserved":preserved,"previous_request_id":previous_request_id,
                    "fresh_request":fresh,"kind_agrees":kind_agrees,"request_chain_agrees":chain_agrees,"telemetry_agrees":telemetry_agrees}),
                    op["expected"]==class&&preserved&&fresh&&kind_agrees&&chain_agrees&&telemetry_agrees)
            }
            _=>panic!("authored operation"),
        };
        events.push(result);
    }
    Ok(())
}
fn main(){
    let args:Vec<String>=std::env::args().collect();PRIVATE.store(args.iter().any(|arg|arg=="--private-diagnostic"),Ordering::Relaxed);
    std::panic::set_hook(Box::new(|_|{}));
    let fixture=fixture();
    if args.get(1).map(String::as_str)==Some("--catalog"){
        println!("{}",json!({"class":"snapshot_catalog","source_commit":COMMIT,"manifest_sha256":FIXTURE_HASH,
            "cases":fixture["cases"].as_array().unwrap().iter().enumerate().map(|(index,case)|json!({"index":index,"id":case["id"]})).collect::<Vec<_>>()}));return;
    }
    assert_eq!(args.get(1).map(String::as_str),Some("--case"));
    let index:usize=args[2].parse().unwrap();let case=&fixture["cases"][index];let mut events=Vec::new();
    let attempted=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||run(case,&fixture,&mut events)));
    let error=match attempted{Ok(Ok(()))=>Value::Null,Ok(Err(error))=>json!({"stage":events.len(),"detail_hash":diagnostic(error)}),
        Err(_)=>json!({"stage":events.len(),"detail_hash":hash(b"owned unexpected panic")})};
    let status=if !error.is_null(){"probe_error"}else if events.iter().any(|event|event["matches"]==false){"difference"}else{"pass"};
    let body=json!({"events":events,"error":error});
    println!("{}",json!({"id":case["id"],"status":status,"hash":digest(&body),"body":body}));
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]fn frozen_manifest(){let fixture=fixture();for case in fixture["cases"].as_array().unwrap(){assert!(case["ops"].as_array().unwrap().len()<100);}}
    #[test]fn typed_literals(){assert!(same(&LiteralValue::Number(7.0),&json!({"kind":"number","value":7.0})));assert!(!same(&LiteralValue::Boolean(true),&json!({"kind":"number","value":1.0})));}
    fn ordinary(deferred:bool){let mut b=book(deferred);b.add_sheet("Calc").unwrap();b.set_value("Calc",1,1,LiteralValue::Number(2.0)).unwrap();
        b.set_formula("Calc",1,2,"=A1*2").unwrap();assert_eq!(b.evaluate_cell("Calc",1,2).unwrap(),LiteralValue::Number(4.0));
        assert_eq!(format!("{:?}",b.engine().inspect_cell_result(&CellAddress::new("Calc",1,2).unwrap()).unwrap().staleness),"Current");}
    #[test]fn ordinary_eager(){ordinary(false);}
    #[test]fn ordinary_deferred(){ordinary(true);}
}
