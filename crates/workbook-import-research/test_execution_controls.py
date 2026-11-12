"""Adversarial execution-receipt and process controls; no Rust engine execution."""
import argparse
from contextlib import redirect_stdout
from copy import deepcopy
import io
import json
from pathlib import Path
import subprocess
import tempfile
from unittest.mock import patch

import run_execution_probe as probe

ROOT = Path(__file__).resolve().parent
MANIFEST = probe.load_manifest()
CASE = MANIFEST["cases"][0]
CANARY = "PRIVATE_RAW_PAYLOAD_MUST_NOT_REACH_CALLER"
CONTROLS = []


def require(condition):
    if not condition:
        raise AssertionError("control expectation")


def check(identity, action):
    try:
        action()
    except Exception as error:
        CONTROLS.append({"id": identity, "status": "failed", "class": type(error).__name__, "hash": probe.sha(str(error).encode())})
    else:
        CONTROLS.append({"id": identity, "status": "passed"})


def pack(case, body, status="pass"):
    return {"id": case["id"], "status": status, "class": case["kind"], "hash": probe.hashed(body), "body": body}


def good(case=CASE):
    events = []
    for identity, phase in probe.expected_layout(case, MANIFEST):
        observed = phase == "cache_before"
        expected = probe.event_expected(case, MANIFEST, identity, phase) if phase != "request" else None
        current = phase in {"baseline", "after_eval", "retry"} or phase == "cache_after" and case.get("expected") in {"success", "cell_error"} and case.get("route") != "prepare"
        if phase == "request":
            classification = "resource:" + case["reason"] if case["expected"] == "resource" else "cancelled" if case["expected"] == "cancelled" else "success:cell_error:" + case["reason"] if case["expected"] == "cell_error" else "preparation_success" if case["route"] == "prepare" else "success:Number"
            event = {"id": identity, "phase": phase, "class": classification, "hash": "a" * 64, "matches": None if case["expected"] == "observe" else True,
                     "present": None, "staleness": None, "cache_matches_expected": None, "inspection_agrees": None}
        else:
            value = float(expected["value"]) if current and expected["kind"] == "number" else None
            classification = "cell_error:" + expected["value"] if current and expected["kind"] == "error" else "Number" if current else "Empty"
            event = {"id": identity, "phase": phase, "class": classification,
                     "hash": probe.hashed({"type": "Number", "value": value}) if value is not None else "b" * 64,
                     "matches": None if observed or case.get("expected") == "observe" and phase == "cache_after" else True,
                     "present": current, "staleness": "Current" if current else "NeverEvaluated", "cache_matches_expected": True if current else False if expected else None, "inspection_agrees": True}
        events.append(event)
    handler = case.get("cancel") in {"handler-value", "handler-error", "error-value-only", "deadline-handler"}
    stats = {"request_id":1,"kind":"Cell","outcome":"Success","work_limit":1000000,"work_charged":1,"scratch_limit":67108864,"scratch_peak":20,"retained_peak":10,"exhaustion":None}
    body = {"events": events, "config_preserved": None if case["kind"] == "mutation" else True, "request_telemetry": None if case["kind"] == "mutation" else stats.copy(), "retry_telemetry": stats.copy(), "handler_calls": [1, 1] if handler else [0, 0], "exercised": True}
    return pack(case, body, "observed" if case.get("expected") == "observe" else "pass")


def reject(record, case=CASE):
    try:
        probe.validate(record, case, MANIFEST)
    except (ValueError, TypeError, KeyError):
        return
    raise AssertionError("malformed receipt admitted")


def mutate_record(action, case=CASE):
    record = good(case)
    action(record)
    record["hash"] = probe.hashed(record["body"])
    reject(record, case)


class Process:
    def __init__(self, running=False, code=0):
        self.running, self.returncode = running, None if running else code
        self.killed = self.waited = False

    def poll(self):
        return None if self.running else self.returncode

    def kill(self):
        self.killed, self.running, self.returncode = True, False, -9

    def wait(self, timeout):
        self.waited = True
        return self.returncode


def worker_control(payload, reason=None, running=False, clock=None, code=0, stderr=b""):
    process = Process(running, code)
    def launch(args, **kwargs):
        require(args[1:] == ["--case", "0"] and "--private-diagnostic" not in args)
        kwargs["stdout"].write(payload); kwargs["stdout"].flush()
        kwargs["stderr"].write(stderr); kwargs["stderr"].flush()
        return process
    with tempfile.TemporaryDirectory(prefix="execution-fault-") as directory:
        output = io.StringIO()
        with patch.object(probe.subprocess, "Popen", side_effect=launch), patch.object(probe.time, "monotonic", side_effect=clock or [0, 0]), redirect_stdout(output):
            record = probe.worker(Path("mock-not-native"), 0, Path(directory), CASE, MANIFEST)
        require(process.waited and process.killed == running and CANARY not in output.getvalue())
        require(record == good() if reason is None else record["status"] == "probe_error" and record["body"]["stage"] == reason)
        probe.validate(record, CASE, MANIFEST)


def cohort_control(mode, expected_exit):
    invoked = []
    with tempfile.TemporaryDirectory(prefix="execution-cohort-") as directory:
        work = Path(directory)
        binary = work / "mock-not-rust.bin"
        binary.write_bytes(b"only a diagnostic control")
        def worker(_binary, index, _temporary, case, _manifest):
            invoked.append(index)
            record = good(case)
            if index == 0:
                if mode == "exception":
                    raise subprocess.SubprocessError(CANARY)
                if mode == "raw":
                    record["actual"] = CANARY
                if mode == "difference":
                    record["body"]["events"][1]["matches"] = False
                    record = pack(case, record["body"], "difference")
                if mode == "unqualified":
                    record["body"]["exercised"] = False
                    record = pack(case, record["body"], "unqualified")
                if mode == "probe_error":
                    record = probe.fallback(case, "worker_exit", probe.sha(CANARY.encode()))
            return record
        output = io.StringIO()
        with patch.object(probe, "worker", side_effect=worker), redirect_stdout(output):
            code = probe.collect(binary, work / "results", MANIFEST)
        report = json.loads((work / "results/results.private.json").read_text())
        journal = [json.loads(line) for line in (work / "results/cases.private.jsonl").read_text().splitlines()]
        require(invoked == list(range(41)) and len(journal) == 41 and journal == report["cases"] and code == expected_exit)
        require([record["id"] for record in journal] == [case["id"] for case in MANIFEST["cases"]])
        require(CANARY not in output.getvalue() and CANARY not in json.dumps(report))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--result-file", type=Path, required=True)
    args = parser.parse_args()
    for case in MANIFEST["cases"]:
        check("valid-" + case["id"], lambda case=case: require(probe.validate(good(case), case, MANIFEST) == good(case)))
    for identity, mutation in [
        ("wrong-id", lambda r:r.update(id="foreign")), ("raw-extra", lambda r:r.update(actual=CANARY)),
        ("raw-class", lambda r:r.update(**{"class":CANARY})), ("uppercase-hash", lambda r:r.update(hash="A"*64))]:
        record=good(); mutation(record)
        check(identity, lambda record=record: reject(record))
    for identity, action in [
        ("raw-body", lambda r:r["body"].update(actual=CANARY)),
        ("raw-event", lambda r:r["body"]["events"][0].update(actual=CANARY)),
        ("wrong-event-order", lambda r:r["body"]["events"].reverse()),
        ("missing-event", lambda r:r["body"]["events"].pop()),
        ("false-aggregate-pass", lambda r:r["body"]["events"][1].update(matches=False)),
        ("false-request-class-match", lambda r:r["body"]["events"][1].update(**{"class":"cancelled"})),
        ("no-request-verification", lambda r:r["body"]["events"][1].update(matches=None)),
        ("no-request-cache-verification", lambda r:r["body"]["events"][2].update(matches=None)),
        ("false-cache-promotion", lambda r:r["body"]["events"][0].update(matches=True)),
        ("dirty-result-promoted", lambda r:r["body"]["events"][-1].update(staleness="Dirty")),
        ("unverified-result-promoted", lambda r:r["body"]["events"][-1].update(cache_matches_expected=False)),
        ("absent-result-promoted", lambda r:r["body"]["events"][-1].update(present=False)),
        ("absent-request-cache-promoted", lambda r:r["body"]["events"][2].update(present=False)),
        ("inspection-disagrees", lambda r:r["body"]["events"][-1].update(inspection_agrees=False)),
        ("wrong-scalar-hash", lambda r:r["body"]["events"][-1].update(hash="d"*64)),
        ("bool-as-number", lambda r:r["body"]["events"][-1].update(**{"class":"Boolean"})),
        ("missing-verification", lambda r:r["body"]["events"][-1].update(matches=None)),
        ("bool-handler-count", lambda r:r["body"].update(handler_calls=[True,0])),
        ("missing-handler", lambda r:r["body"].update(handler_calls=[0])),
        ("raw-handler", lambda r:r["body"].update(handler_calls=[CANARY,0])),
        ("unpreserved-config-pass", lambda r:r["body"].update(config_preserved=False)),
        ("missing-request-telemetry", lambda r:r["body"].update(request_telemetry=None)),
        ("missing-retry-telemetry", lambda r:r["body"].update(retry_telemetry=None)),
        ("fake-exercised", lambda r:r["body"].update(exercised=False)),
        ("raw-event-class", lambda r:r["body"]["events"][1].update(**{"class":CANARY})),
        ("list-event-class", lambda r:r["body"]["events"][1].update(**{"class":[]})),
    ]:
        check(identity, lambda action=action: mutate_record(action))
    handler_case = next(case for case in MANIFEST["cases"] if case.get("cancel") == "handler-value")
    check("missing-retry-handler-exercise", lambda: mutate_record(lambda r:r["body"].update(handler_calls=[1,0]), handler_case))
    for field, value in [("work_charged",True),("work_charged",None),("work_limit",-1),("kind",CANARY),("outcome",CANARY),("exhaustion",CANARY),("scratch_peak",2**64)]:
        def action(record,field=field,value=value):
            stats={"request_id":1,"kind":"Cell","outcome":"Success","work_limit":1000,"work_charged":1,"scratch_limit":1000,"scratch_peak":20,"retained_peak":10,"exhaustion":None}
            stats[field]=value
            record["body"]["request_telemetry"]=stats
        check("telemetry-"+field+"-"+type(value).__name__,lambda action=action:mutate_record(action))
    payload=json.dumps(good()).encode()
    for identity, arguments in [
        ("worker-valid",{}), ("worker-raw",{"payload":CANARY.encode(),"reason":"worker_receipt_invalid"}),
        ("worker-extra",{"payload":payload+CANARY.encode(),"reason":"worker_receipt_invalid"}),
        ("worker-exit",{"code":17,"reason":"worker_exit"}),
        ("worker-receipt-limit",{"payload":b"x"*65_537,"reason":"worker_receipt_limit"}),
        ("worker-late-running",{"clock":[0,16],"running":True,"reason":"worker_timeout"}),
        ("worker-late-exited",{"clock":[0,16],"reason":"worker_timeout"}),
        ("worker-output-running",{"clock":[0,0],"running":True,"stderr":b"x"*1_000_001,"reason":"worker_output_limit"}),
        ("worker-output-exited",{"stderr":b"x"*1_000_001,"reason":"worker_output_limit"}),
    ]:
        check(identity,lambda arguments=arguments:worker_control(**{"payload":payload,**arguments}))
    for mode,code in [("pass",0),("difference",1),("exception",2),("raw",2),("probe_error",2)]:
        check("cohort-"+mode,lambda mode=mode,code=code:cohort_control(mode,code))
    report={"scope":"Mocked receipt/process controls; no Rust engine observations","controls":CONTROLS,"total":len(CONTROLS),"passed":sum(control["status"]=="passed" for control in CONTROLS),
            "supervisor_sha256":probe.file_sha(ROOT/"run_execution_probe.py"),"controls_source_sha256":probe.file_sha(Path(__file__))}
    require(len({control["id"] for control in CONTROLS}) == len(CONTROLS))
    with args.result_file.open("x",encoding="utf-8") as out:json.dump(report,out,indent=2)
    print(json.dumps({key:report[key] for key in ["scope","passed","total"]}))
    raise SystemExit(0 if report["passed"]==report["total"] else 1)


if __name__=="__main__":main()
