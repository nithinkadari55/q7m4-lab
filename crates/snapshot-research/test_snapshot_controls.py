"""Mocked protocol/process fault controls; no native engine qualification."""
import argparse
from contextlib import redirect_stdout
from copy import deepcopy
import io
import json
from pathlib import Path
import subprocess
import tempfile
from unittest.mock import patch
import run_snapshot_probe as probe

ROOT=Path(__file__).resolve().parent
MANIFEST=probe.load_manifest()
CASE=MANIFEST["cases"][0]
CANARY="PRIVATE_RAW_PAYLOAD_MUST_NOT_ESCAPE"
CONTROLS=[]

def require(value):
    if not value:raise AssertionError("fault-control expectation")

def check(identity,action):
    try:action()
    except Exception as error:CONTROLS.append({"id":identity,"status":"failed","type":type(error).__name__})
    else:CONTROLS.append({"id":identity,"status":"passed"})

def pack(case,events,error=None):
    body={"events":events,"error":error}
    status="probe_error" if error is not None else "difference" if any(not event["matches"] for event in events) else "pass"
    return {"id":case["id"],"status":status,"hash":probe.hashed(body),"body":body}

def typed(expected):
    if expected is None:return "Empty",{"type":"Empty"}
    if expected["kind"]=="number":return "Number",{"type":"Number","value":float(expected["value"])}
    return "cell_error:"+expected["value"],{"type":"Error","kind":expected["value"],"detail_hash":"e"*64}

def good(case=CASE):
    events=[];current={"mutation_revision":1,"recalc_epoch":0};captured={};request_id=0
    for op in case["ops"]:
        kind=op["kind"];classification="";details={};raw={}
        if kind=="call":
            previous=request_id or None;request_id+=1;current["recalc_epoch"]+=1
            classification=op["expected"]
            expected_kind={"snapshot":"FullCancellable","all":"FullCancellable","cell":"Cell","cells":"CellsCancellable","targets":"Targeted","prepare":"TargetPreparation","plan":"RecalcPlan"}[op["route"]]
            stats={"request_id":request_id,"kind":expected_kind,"outcome":"Success" if classification=="success" else "Cancelled" if classification=="cancelled" else "Error",
                   "work_limit":1000000,"work_charged":1,"scratch_limit":67108864,"scratch_peak":32,"retained_peak":0,
                   "exhaustion":classification.split(":")[1] if classification.startswith("resource:") else None}
            details={"telemetry":stats,"budget_preserved":True,"previous_request_id":previous,"fresh_request":True,"kind_agrees":True,"request_chain_agrees":True,"telemetry_agrees":True}
            raw={"kind":"success"} if classification=="success" else {"detail_hash":"b"*64}
        elif kind=="read":
            classification,raw=typed(op["expected"])
            details={"present":op["expected"] is not None,"staleness":op["states"][0] if op["states"] else "NeverEvaluated",
                     "inspection_agrees":True,"stamp":deepcopy(current),"read_only":True}
        elif kind=="counter":classification="counter";raw=details={"count":op["expected"]}
        elif kind=="capture":
            classification="captured";captured[op["key"]]=deepcopy(current);raw=deepcopy(current);details={"stamp":deepcopy(current)}
        elif kind=="guard":
            classification="same" if captured[op["key"]]==current else "different"
            raw=details={"captured":deepcopy(captured[op["key"]]),"current":deepcopy(current)}
        elif kind=="mutate":
            current["mutation_revision"]+=1;classification="applied";raw={"applied":True};details={"stamp":deepcopy(current)}
        elif kind=="budget":classification="configured";raw=details={"safe":True}
        elif kind=="page":
            classification=op["expected"]
            if classification=="page":
                items=[]
                for expected in op["items"]:
                    item_class,payload=typed(expected["expected"])
                    items.append({"sheet":"Calc","row":1,"col":expected["col"],"class":item_class,"hash":probe.hashed(payload),"present":True,"value_included":True,"staleness":"Current"})
                raw={"stamp":deepcopy(current),"offset":op["offset"],"total":3,"next_offset":2 if op["offset"]==0 else None,"items":items}
                details={"page":raw,"current_stamp":deepcopy(current),"read_only":True,"binding_agrees":True}
            else:raw={"detail_hash":"b"*64};details={"stamp":deepcopy(current),"read_only":True,"binding_agrees":True}
        events.append({"id":op["id"],"kind":kind,"class":classification,"hash":probe.hashed(raw),"details":details,"matches":True})
    return pack(case,events)

def reject(record,case=CASE):
    try:probe.validate(record,case,MANIFEST)
    except (ValueError,TypeError,KeyError):return
    raise AssertionError("malformed receipt admitted")

def mutant(action,case=CASE):
    record=good(case);action(record);record["hash"]=probe.hashed(record["body"]);reject(record,case)

class Process:
    def __init__(self,running=False,code=0):self.running=running;self.returncode=None if running else code;self.killed=False;self.waited=False
    def poll(self):return None if self.running else self.returncode
    def kill(self):self.killed=True;self.running=False;self.returncode=-9
    def wait(self,timeout):self.waited=True;return self.returncode

def worker_control(payload,reason=None,running=False,clock=None,code=0,stderr=b""):
    process=Process(running,code)
    def launch(args,**kwargs):
        require(args[1:]==["--case","0"])
        kwargs["stdout"].write(payload);kwargs["stdout"].flush();kwargs["stderr"].write(stderr);kwargs["stderr"].flush();return process
    with tempfile.TemporaryDirectory(prefix="snapshot-process-control-") as directory:
        output=io.StringIO()
        with patch.object(probe.subprocess,"Popen",side_effect=launch),patch.object(probe.time,"monotonic",side_effect=clock or [0,0]),redirect_stdout(output):
            record=probe.worker(Path("mock-not-native"),0,Path(directory),CASE,MANIFEST)
        require(process.waited and process.killed==running and CANARY not in output.getvalue())
        require(record==good() if reason is None else record["status"]=="probe_error" and record["body"]["error"]["stage"]==reason)
        probe.validate(record,CASE,MANIFEST)

def cohort_control(mode,expected_exit):
    invoked=[]
    with tempfile.TemporaryDirectory(prefix="snapshot-cohort-control-") as directory:
        base=Path(directory);binary=base / "mock-not-native.bin";binary.write_bytes(b"diagnostic mock")
        def worker(_binary,index,_temporary,case,_manifest):
            invoked.append(index);record=good(case)
            if index==0:
                if mode=="exception":raise subprocess.SubprocessError(CANARY)
                if mode=="raw":record["actual"]=CANARY
                if mode=="difference":
                    record["body"]["events"][0]["details"]["budget_preserved"]=False
                    record["body"]["events"][0]["matches"]=False;record=pack(case,record["body"]["events"])
                if mode=="probe_error":record=probe.fallback(case,"worker_exit",probe.sha(CANARY.encode()))
            return record
        output=io.StringIO()
        with patch.object(probe,"worker",side_effect=worker),redirect_stdout(output):code=probe.collect(binary,base / "results",MANIFEST)
        report=json.loads((base / "results/results.private.json").read_bytes())
        journal=[json.loads(line) for line in (base / "results/cases.private.jsonl").read_text().splitlines()]
        require(invoked==list(range(54)) and len(journal)==54 and journal==report["cases"] and code==expected_exit)
        require(CANARY not in output.getvalue() and CANARY not in json.dumps(report))

def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument("--result-file",type=Path,required=True);args=parser.parse_args()
    for case in MANIFEST["cases"]:check("valid-"+case["id"],lambda case=case:require(probe.validate(good(case),case,MANIFEST)==good(case)))
    for identity,action in [("raw-top",lambda r:r.update(actual=CANARY)),("wrong-id",lambda r:r.update(id="foreign")),
                            ("raw-body",lambda r:r["body"].update(actual=CANARY)),("body-digest",lambda r:r.update(hash="a"*64)),
                            ("raw-event",lambda r:r["body"]["events"][0].update(actual=CANARY)),
                            ("missing-operation",lambda r:r["body"]["events"].pop()),("order",lambda r:r["body"]["events"].reverse()),
                            ("false-match",lambda r:r["body"]["events"][0].update(matches=False)),
                            ("raw-call-details",lambda r:r["body"]["events"][0]["details"].update(message=CANARY)),
                            ("absent-promoted",lambda r:r["body"]["events"][1]["details"].update(present=False)),
                            ("dirty-promoted",lambda r:r["body"]["events"][1]["details"].update(staleness="Dirty")),
                            ("wrong-number",lambda r:r["body"]["events"][1].update(hash="b"*64)),
                            ("bool-as-number",lambda r:r["body"]["events"][1].update(**{"class":"Boolean"})),
                            ("inspection-disagrees",lambda r:r["body"]["events"][1]["details"].update(inspection_agrees=False)),
                            ("read-mutates",lambda r:r["body"]["events"][1]["details"].update(read_only=False)),
                            ("bool-stamp",lambda r:r["body"]["events"][1]["details"]["stamp"].update(recalc_epoch=True)),
                            ("missing-telemetry",lambda r:r["body"]["events"][0]["details"].update(telemetry=None)),
                            ("retained-request",lambda r:r["body"]["events"][0]["details"].update(previous_request_id=100)),
                            ("wrong-request-kind",lambda r:r["body"]["events"][0]["details"]["telemetry"].update(kind="Cell")),
                            ("incomplete-success",lambda r:r["body"]["events"][0]["details"]["telemetry"].update(outcome="InProgress"))]:
        if identity=="body-digest":
            record=good();action(record);check(identity,lambda record=record:reject(record));continue
        check(identity,lambda action=action:mutant(action))
    next_case=next(case for case in MANIFEST["cases"] if case["category"]=="next_request")
    for kind in ["read","call","counter","capture","guard","mutate","budget","page"]:
        case=next(case for case in MANIFEST["cases"] if any(op["kind"]==kind for op in case["ops"]))
        index=next(i for i,op in enumerate(case["ops"]) if op["kind"]==kind)
        check("details-private-"+kind,lambda case=case,index=index:mutant(lambda r:r["body"]["events"][index]["details"].update(raw=CANARY),case))
    for identity,kind,action in [
        ("counter-forgery","counter",lambda e:e["details"].update(count=True)),
        ("capture-digest","capture",lambda e:e.update(hash="b"*64)),
        ("guard-forged-capture","guard",lambda e:e["details"]["captured"].update(mutation_revision=999)),
        ("page-extra-raw","page",lambda e:e["details"]["page"].update(raw=CANARY)),
        ("page-extra-item","page",lambda e:e["details"]["page"]["items"].append(deepcopy(e["details"]["page"]["items"][0]))),
        ("page-wrong-value","page",lambda e:e["details"]["page"]["items"][0].update(hash="b"*64)),
        ("page-wrong-coordinate","page",lambda e:e["details"]["page"]["items"][0].update(col=True)),
    ]:
        index=next(i for i,op in enumerate(next_case["ops"]) if op["kind"]==kind)
        check(identity,lambda action=action,index=index:mutant(lambda r:action(r["body"]["events"][index]),next_case))
    for identity,action in [("page-foreign-stamp",lambda e:e["details"]["page"]["stamp"].update(mutation_revision=999)),
                            ("call-unchained-previous",lambda e:e["details"].update(previous_request_id=None))]:
        kind="page" if identity.startswith("page") else "call"
        index=next(i for i,op in enumerate(next_case["ops"]) if op["kind"]==kind and (kind=="page" or i>0))
        def tamper(record,index=index,action=action):
            event=record["body"]["events"][index];action(event)
            if event["kind"]=="page":event["hash"]=probe.hashed(event["details"]["page"])
        check(identity,lambda tamper=tamper:mutant(tamper,next_case))
    resource=next(case for case in MANIFEST["cases"] if case["id"].endswith("/work_zero"))
    cancelled=next(case for case in MANIFEST["cases"] if case["id"].endswith("/pre_cancel"))
    for identity,case,changes in [("resource-success-telemetry",resource,{"outcome":"Success","exhaustion":None}),
                                  ("resource-wrong-reason",resource,{"exhaustion":"deadline"}),
                                  ("cancel-inprogress",cancelled,{"outcome":"InProgress"})]:
        index=next(i for i,op in enumerate(case["ops"]) if op["kind"]=="call" and op["expected"]!="success")
        check(identity,lambda case=case,index=index,changes=changes:mutant(lambda r:r["body"]["events"][index]["details"]["telemetry"].update(changes),case))
        def retained(case=case,index=index,changes=changes):
            record=good(case);event=record["body"]["events"][index];event["details"]["telemetry"].update(changes)
            event["details"]["telemetry_agrees"]=False;event["matches"]=False;record=pack(case,record["body"]["events"])
            require(probe.validate(record,case,MANIFEST)==record and record["status"]=="difference")
        check(identity+"-retained-as-difference",retained)
    mismatch_index=next(i for i,op in enumerate(next_case["ops"]) if op["kind"]=="page" and op["expected"]=="revision_mismatch")
    capture_index=next(i for i,op in enumerate(next_case["ops"]) if op["kind"]=="capture")
    def false_mismatch(record):
        record["body"]["events"][mismatch_index]["details"]["stamp"]=deepcopy(record["body"]["events"][capture_index]["details"]["stamp"])
    check("page-false-revision-mismatch",lambda:mutant(false_mismatch,next_case))
    def retained_page_difference():
        record=good(next_case);false_mismatch(record);event=record["body"]["events"][mismatch_index]
        event["details"]["binding_agrees"]=False;event["matches"]=False;record=pack(next_case,record["body"]["events"])
        require(probe.validate(record,next_case,MANIFEST)==record and record["status"]=="difference")
    check("page-false-mismatch-retained-as-difference",retained_page_difference)
    payload=json.dumps(good()).encode()
    for identity,arguments in [("worker-valid",{}),("worker-invalid",{"payload":CANARY.encode(),"reason":"worker_receipt_invalid"}),
        ("worker-extra",{"payload":payload+CANARY.encode(),"reason":"worker_receipt_invalid"}),("worker-exit",{"code":7,"reason":"worker_exit"}),
        ("worker-receipt-cap",{"payload":b"x"*65537,"reason":"worker_receipt_limit"}),
        ("worker-running-timeout",{"running":True,"clock":[0,16],"reason":"worker_timeout"}),
        ("worker-exited-timeout",{"clock":[0,16],"reason":"worker_timeout"}),
        ("worker-running-output",{"running":True,"clock":[0,0],"stderr":b"x"*1000001,"reason":"worker_output_limit"}),
        ("worker-exited-output",{"stderr":b"x"*1000001,"reason":"worker_output_limit"})]:
        check(identity,lambda arguments=arguments:worker_control(**{"payload":payload,**arguments}))
    for mode,code in [("pass",0),("difference",1),("exception",2),("raw",2),("probe_error",2)]:check("cohort-"+mode,lambda mode=mode,code=code:cohort_control(mode,code))
    report={"scope":"Mocked protocol/process controls; not native engine results","controls":CONTROLS,"total":len(CONTROLS),"passed":sum(c["status"]=="passed" for c in CONTROLS),
            "supervisor_sha256":probe.file_sha(ROOT / "run_snapshot_probe.py"),"controls_source_sha256":probe.file_sha(Path(__file__))}
    require(len({c["id"] for c in CONTROLS})==len(CONTROLS))
    with args.result_file.open("x",encoding="utf-8") as out:json.dump(report,out,indent=2)
    print(json.dumps({key:report[key] for key in ["scope","passed","total"]}))
    raise SystemExit(0 if report["passed"]==report["total"] else 1)

if __name__=="__main__":main()
