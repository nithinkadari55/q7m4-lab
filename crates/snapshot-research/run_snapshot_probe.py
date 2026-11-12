"""Bounded source-pinned execution research supervisor; not a production service."""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parent
MANIFEST_HASH = "a02136889d55e451fa80a345a027223f555ad1a366fd83d86104af59f03ec056"
ERRORS = {"#NULL!", "#REF!", "#NAME?", "#VALUE!", "#DIV/0!", "#N/A", "#NUM!", "#ERROR!", "#N/IMPL!", "#SPILL!", "#CALC!", "#CIRC!", "#CANCELLED!"}
REASONS = {"admission", "retained_memory", "scratch_memory", "work_units", "deadline", "graph_vertices", "graph_edges", "materialization_cells", "arithmetic_overflow"}
VALUE_CLASSES = {"Number", "Int", "Text", "Boolean", "Empty", "Other"} | {"cell_error:" + error for error in ERRORS}
REQUEST_CLASSES = {"preparation_success", "cancelled", "io_error"} | {"resource:" + reason for reason in REASONS} | {"api_error:" + error for error in ERRORS} | {"success:" + classification for classification in VALUE_CLASSES}
EVENT_FIELDS = {"id", "phase", "class", "hash", "matches", "present", "staleness", "cache_matches_expected", "inspection_agrees"}
BODY_FIELDS = {"events", "config_preserved", "request_telemetry", "retry_telemetry", "handler_calls", "exercised"}
TELEMETRY_FIELDS = {"request_id", "kind", "outcome", "work_limit", "work_charged", "scratch_limit", "scratch_peak", "retained_peak", "exhaustion"}
STAGES = {"owned_case", "worker_timeout", "worker_output_limit", "worker_exit", "worker_receipt_limit", "worker_receipt_invalid", "supervisor_exception"}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def hashed(value):
    return sha(json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode())


def file_sha(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def is_hash(value):
    return type(value) is str and len(value) == 64 and all(char in "0123456789abcdef" for char in value)


def nullable_bool(value):
    return value is None or type(value) is bool


def load_manifest():
    raw = (ROOT / "snapshot-fixtures.json").read_bytes()
    if len(raw) > 1024 * 1024 or sha(raw) != MANIFEST_HASH:
        raise ValueError("fixture identity/size")
    manifest = json.loads(raw)
    if manifest["source_commit"] != "dfd2f96457ff0929114a3e6b786c2761e6db5c5d" or len(manifest["cases"]) != 54 or len(manifest["models"]) != 6:
        raise ValueError("fixture scope")
    if len({case["id"] for case in manifest["cases"]}) != 54:
        raise ValueError("duplicate case")
    return manifest


def telemetry(value):
    if value is None:
        return
    if type(value) is not dict or set(value) != TELEMETRY_FIELDS:
        raise ValueError("telemetry fields")
    kinds = {"Vertex", "Targeted", "TargetPreparation", "RecalcPlan", "Full", "FullWithDelta", "Cell", "Cells", "CellsCancellable", "CellsWithDelta", "FullCancellable", "TargetedCancellable", "FullLogged"}
    if type(value["kind"]) is not str or value["kind"] not in kinds or type(value["outcome"]) is not str or value["outcome"] not in {"InProgress", "Success", "Cancelled", "Error"}:
        raise ValueError("telemetry classification")
    if value["exhaustion"] is not None and (type(value["exhaustion"]) is not str or value["exhaustion"] not in REASONS):
        raise ValueError("exhaustion classification")
    for field in TELEMETRY_FIELDS - {"kind", "outcome", "exhaustion"}:
        if field in {"request_id", "work_charged", "scratch_peak", "retained_peak"} and value[field] is None:
            raise ValueError("required telemetry integer")
        if value[field] is not None and (type(value[field]) is not int or not 0 <= value[field] < 2**64):
            raise ValueError("telemetry integer")


def integer(value, nullable=False):
    return nullable and value is None or type(value) is int and 0 <= value < 2**64


def state_stamp(value):
    if type(value) is not dict or set(value) != {"mutation_revision","recalc_epoch"} or not all(integer(item) for item in value.values()):
        raise ValueError("state stamp")


def expected_value(classification, digest, expected):
    if expected is None:
        return True
    if expected["kind"] == "number":
        if classification not in {"Number","Int"}:
            return False
        value = float(expected["value"]) if classification == "Number" else int(expected["value"])
        return digest == hashed({"type":classification,"value":value})
    if expected["kind"] == "error":
        return classification == "cell_error:"+expected["value"]
    raise ValueError("authored expected type")


def validate(record,case,manifest):
    if type(record) is not dict or set(record)!={"id","status","hash","body"} or record["id"]!=case["id"] or not is_hash(record["hash"]):
        raise ValueError("receipt identity/schema")
    body=record["body"]
    if type(body) is not dict or set(body)!={"events","error"} or hashed(body)!=record["hash"]:
        raise ValueError("body digest/schema")
    events,error=body["events"],body["error"]
    if type(events) is not list or len(events)>len(case["ops"]):
        raise ValueError("event bounds")
    if error is not None:
        if type(error) is not dict or set(error)!={"stage","detail_hash"} or not is_hash(error["detail_hash"]):
            raise ValueError("failure attribution")
        stage=error["stage"]
        if not (type(stage) is int and stage==len(events) or type(stage) is str and stage in STAGES and not events):
            raise ValueError("failure stage")
    elif len(events)!=len(case["ops"]):
        raise ValueError("missing operations")
    captured={};last_request_id=None
    for event,op in zip(events,case["ops"]):
        if type(event) is not dict or set(event)!={"id","kind","class","hash","details","matches"}:
            raise ValueError("event fields")
        if event["id"]!=op["id"] or event["kind"]!=op["kind"] or type(event["class"]) is not str or not is_hash(event["hash"]) or type(event["matches"]) is not bool or type(event["details"]) is not dict:
            raise ValueError("event identity/types")
        classification,details,kind=event["class"],event["details"],op["kind"]
        if kind=="read":
            if classification not in VALUE_CLASSES or set(details)!={"present","staleness","inspection_agrees","stamp","read_only"}:
                raise ValueError("read classification/schema")
            if any(type(details[key]) is not bool for key in ["present","inspection_agrees","read_only"]) or details["staleness"] not in {"Current","Dirty","NeverEvaluated"}:
                raise ValueError("read metadata")
            state_stamp(details["stamp"])
            value_ok=expected_value(classification,event["hash"],op["expected"])
            value_ok &= op["expected"] is None or details["present"]
            matched=value_ok and (not op["states"] or details["staleness"] in op["states"]) and details["inspection_agrees"] and details["read_only"]
        elif kind=="call":
            if classification not in REQUEST_CLASSES|{"success","unwind"} or set(details)!={"telemetry","budget_preserved","previous_request_id","fresh_request","kind_agrees","request_chain_agrees","telemetry_agrees"} or any(type(details[key]) is not bool for key in ["budget_preserved","fresh_request","kind_agrees","request_chain_agrees","telemetry_agrees"]) or not integer(details["previous_request_id"],True):
                raise ValueError("call classification/schema")
            telemetry(details["telemetry"])
            if classification!="unwind" and details["telemetry"] is None:
                raise ValueError("missing completed request telemetry")
            if classification=="success" and event["hash"]!=hashed({"kind":"success"}):
                raise ValueError("success attribution")
            stats=details["telemetry"]
            fresh=stats is not None and (details["previous_request_id"] is None or stats["request_id"]>details["previous_request_id"])
            expected_kind={"snapshot":"FullCancellable","all":"FullCancellable","cell":"Cell","cells":"CellsCancellable","targets":"Targeted","prepare":"TargetPreparation","plan":"RecalcPlan"}[op["route"]]
            kind_agrees=stats is not None and stats["kind"]==expected_kind
            chain_agrees=last_request_id is None or details["previous_request_id"]==last_request_id
            last_request_id=stats["request_id"] if stats else None
            outcome=stats["outcome"] if stats else None;exhaustion=stats["exhaustion"] if stats else None
            telemetry_agrees=classification=="success" and outcome=="Success" and exhaustion is None or classification=="cancelled" and outcome=="Cancelled" and exhaustion is None or classification.startswith("resource:") and outcome=="Error" and exhaustion==classification.split(":",1)[1]
            if details["fresh_request"]!=fresh or details["kind_agrees"]!=kind_agrees or details["request_chain_agrees"]!=chain_agrees or details["telemetry_agrees"]!=telemetry_agrees:raise ValueError("request telemetry binding")
            matched=classification==op["expected"] and details["budget_preserved"] and fresh and kind_agrees and chain_agrees and telemetry_agrees
        elif kind=="counter":
            if classification!="counter" or set(details)!={"count"} or not integer(details["count"]) or details["count"]>1000 or event["hash"]!=hashed(details):
                raise ValueError("counter attribution")
            matched=details["count"]==op["expected"]
        elif kind=="capture":
            if classification!="captured" or set(details)!={"stamp"}:
                raise ValueError("capture schema")
            state_stamp(details["stamp"])
            if event["hash"]!=hashed(details["stamp"]):raise ValueError("capture digest")
            captured[op["key"]]=details["stamp"]
            matched=True
        elif kind=="guard":
            if classification not in {"same","different"} or set(details)!={"captured","current"}:
                raise ValueError("guard schema")
            state_stamp(details["captured"]);state_stamp(details["current"])
            if details["captured"]!=captured[op["key"]] or event["hash"]!=hashed(details):raise ValueError("guard identity")
            relation="same" if details["captured"]==details["current"] else "different"
            if classification!=relation:raise ValueError("guard relation")
            matched=op["expected"]=="observe" or classification==op["expected"]
        elif kind=="mutate":
            if classification not in REQUEST_CLASSES|{"applied"} or set(details)!={"stamp"}:raise ValueError("mutation schema")
            state_stamp(details["stamp"])
            if classification=="applied" and event["hash"]!=hashed({"applied":True}):raise ValueError("mutation attribution")
            matched=classification=="applied"
        elif kind=="budget":
            if classification!="configured" or set(details)!={"safe"} or type(details["safe"]) is not bool or event["hash"]!=hashed({"safe":True}):raise ValueError("budget schema")
            matched=details["safe"]
        elif kind=="page":
            if classification not in {"page","revision_mismatch","inspection_error"} or type(details.get("read_only")) is not bool or type(details.get("binding_agrees")) is not bool:raise ValueError("page classification")
            matched=classification==op["expected"] and details["read_only"]
            if classification=="page":
                if set(details)!={"page","current_stamp","read_only","binding_agrees"}:raise ValueError("page schema")
                page=details["page"]
                if type(page) is not dict or set(page)!={"stamp","offset","total","next_offset","items"} or event["hash"]!=hashed(page):raise ValueError("page digest/schema")
                state_stamp(page["stamp"])
                state_stamp(details["current_stamp"])
                binding=page["stamp"]==captured[op["key"]] and page["stamp"]==details["current_stamp"]
                if not all(integer(page[key],key=="next_offset") for key in ["offset","total","next_offset"]) or type(page["items"]) is not list or len(page["items"])>op["limit"]:raise ValueError("page bounds")
                for item in page["items"]:
                    if type(item) is not dict or set(item)!={"sheet","row","col","class","hash","present","value_included","staleness"}:raise ValueError("page item schema")
                    if item["sheet"]!="Calc" or type(item["row"]) is not int or item["row"]!=1 or type(item["col"]) is not int or not 1<=item["col"]<=3 or item["class"] not in VALUE_CLASSES or not is_hash(item["hash"]):raise ValueError("page item identity")
                    if any(type(item[key]) is not bool for key in ["present","value_included"]) or item["staleness"] not in {"Current","Dirty","NeverEvaluated"}:raise ValueError("page item flags")
                matched &= page["total"]==3 and page["offset"]==op["offset"] and page["next_offset"]==(2 if op["offset"]==0 else None) and len(page["items"])==len(op["items"])
                matched &= all(item["col"]==expected["col"] and item["present"] and item["value_included"] and item["staleness"]=="Current" and expected_value(item["class"],item["hash"],expected["expected"]) for item,expected in zip(page["items"],op["items"]))
            else:
                if set(details)!={"stamp","read_only","binding_agrees"}:raise ValueError("page error schema")
                state_stamp(details["stamp"])
                binding=classification!="revision_mismatch" or details["stamp"]!=captured[op["key"]]
            if details["binding_agrees"]!=binding:raise ValueError("page stamp binding")
            matched &= binding
        else:raise ValueError("authored operation")
        if event["matches"]!=bool(matched):raise ValueError("operation expectation binding")
    expected_status="probe_error" if error is not None else "difference" if any(not event["matches"] for event in events) else "pass"
    if type(record["status"]) is not str or record["status"]!=expected_status:raise ValueError("aggregate status")
    return record


def fallback(case,stage,digest):
    body={"events":[],"error":{"stage":stage,"detail_hash":digest}}
    return {"id":case["id"],"status":"probe_error","hash":hashed(body),"body":body}

def worker(binary, index, temporary, case, manifest):
    stdout_path, stderr_path = temporary / (str(index) + ".stdout.private"), temporary / (str(index) + ".stderr.private")
    started, reason = time.monotonic(), None
    with stdout_path.open("xb") as out, stderr_path.open("xb") as error:
        process = subprocess.Popen([str(binary), "--case", str(index)], cwd=temporary, stdout=out, stderr=error)
        try:
            while process.poll() is None:
                if time.monotonic() - started > 15:
                    reason = "worker_timeout"
                    break
                if stdout_path.stat().st_size + stderr_path.stat().st_size > 1_000_000:
                    reason = "worker_output_limit"
                    break
                time.sleep(0.02)
            if reason is None and time.monotonic() - started > 15:
                reason = "worker_timeout"
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=5)
    if reason is None and stdout_path.stat().st_size + stderr_path.stat().st_size > 1_000_000:
        reason = "worker_output_limit"
    if reason:
        return fallback(case, reason, file_sha(stdout_path))
    if process.returncode != 0:
        return fallback(case, "worker_exit", file_sha(stdout_path))
    with stdout_path.open("rb") as stream:
        raw = stream.read(65_537)
    if len(raw) > 65_536:
        return fallback(case, "worker_receipt_limit", file_sha(stdout_path))
    try:
        return validate(json.loads(raw), case, manifest)
    except (ValueError, TypeError, KeyError):
        return fallback(case, "worker_receipt_invalid", sha(raw))


def collect(binary, destination, manifest):
    destination.mkdir(exist_ok=False)
    records = []
    with (destination / "cases.private.jsonl").open("x", encoding="utf-8") as journal, tempfile.TemporaryDirectory(prefix="workers-", dir=destination) as directory:
        for index, case in enumerate(manifest["cases"]):
            try:
                record = validate(worker(binary, index, Path(directory), case, manifest), case, manifest)
            except (OSError, subprocess.SubprocessError, ValueError, TypeError, KeyError) as error:
                record = fallback(case, "supervisor_exception", sha(type(error).__name__.encode()))
            records.append(record)
            journal.write(json.dumps(record) + "\n")
            journal.flush()
            print(json.dumps(record), flush=True)
    report = {"source_commit": manifest["source_commit"], "manifest_sha256": MANIFEST_HASH, "binary_sha256": file_sha(binary),
              "cases": records, "counts": dict(Counter(record["status"] for record in records)), "scope": "Finite snapshots, targeted continuation, paged reads and failure/retry; not hard containment or production qualification"}
    result = destination / "results.private.json"
    with result.open("x", encoding="utf-8") as out:
        json.dump(report, out, indent=2)
    print(json.dumps({"class": "complete_snapshot_cases", "cases": len(records), "counts": report["counts"], "hash": file_sha(result)}), flush=True)
    return 2 if any(record["status"] == "probe_error" for record in records) else 1 if any(record["status"] in {"difference", "unqualified"} for record in records) else 0


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--result-dir", type=Path, required=True)
    args = parser.parse_args()
    raise SystemExit(collect(args.binary.resolve(strict=True), args.result_dir.resolve(), load_manifest()))
