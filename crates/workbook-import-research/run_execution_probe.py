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
MANIFEST_HASH = "0f28ec5c0fbf86a0545afd523200b2e4864841f3e159371177c02f43ba51cdd7"
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
    raw = (ROOT / "execution-fixtures.json").read_bytes()
    if len(raw) > 1024 * 1024 or sha(raw) != MANIFEST_HASH:
        raise ValueError("fixture identity/size")
    manifest = json.loads(raw)
    if manifest["source_commit"] != "dfd2f96457ff0929114a3e6b786c2761e6db5c5d" or len(manifest["cases"]) != 41 or len(manifest["models"]) != 6:
        raise ValueError("fixture scope")
    if len({case["id"] for case in manifest["cases"]}) != 41:
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


def expected_layout(case, manifest):
    if case["kind"] == "mutation":
        model = next(model for model in manifest["models"] if model["id"] == case["model_id"])
        layout = [("initial/" + str(index), "baseline") for index, _ in enumerate(model["initial_targets"])]
        for action in model["actions"]:
            for index, _ in enumerate(action["targets"]):
                identity = action["id"] + "/" + str(index)
                layout += [(identity, "cache_before"), (identity, "after_eval")]
        return layout
    layout = [("target", "cache_before"), ("request", "request"), ("target", "cache_after")]
    if case["expected"] in {"resource", "cancelled"}:
        layout.append(("precedent", "cache_after"))
    return layout + [("target", "retry")]


def event_expected(case, manifest, identity, phase):
    if case["kind"] == "mutation":
        model = next(model for model in manifest["models"] if model["id"] == case["model_id"])
        action, index = identity.rsplit("/", 1)
        targets = model["initial_targets"] if action == "initial" else next(item["targets"] for item in model["actions"] if item["id"] == action)
        return targets[int(index)]["expected"]
    if identity == "precedent":
        return None
    if phase == "retry":
        return {"kind": "number", "value": 30.0 if case.get("array") else 18.0}
    if phase == "cache_after" and case["expected"] == "cell_error":
        return {"kind": "error", "value": case["reason"]}
    return {"kind": "number", "value": 26.0 if case.get("array") else 14.0}


def bind_match(event, expected):
    if expected is None:
        return
    classification = event["class"]
    compatible = {"number": {"Number", "Int"}, "text": {"Text"}, "boolean": {"Boolean"}, "empty": {"Empty"}, "error": {"cell_error:" + str(expected["value"])}}
    if classification not in compatible[expected["kind"]]:
        raise ValueError("matched result type")
    # These finite targets are small exact integers, not a general float/ULP
    # contract. Independently bind a claimed matching payload to its type/hash.
    if expected["kind"] == "number" and float(expected["value"]).is_integer():
        value = float(expected["value"]) if classification == "Number" else int(expected["value"])
        if event["hash"] != hashed({"type": classification, "value": value}):
            raise ValueError("matched scalar payload")


def validate(record, case, manifest):
    if type(record) is not dict or set(record) != {"id", "status", "class", "hash", "body"}:
        raise ValueError("receipt fields")
    if record["id"] != case["id"] or record["class"] != case["kind"] or not is_hash(record["hash"]):
        raise ValueError("receipt identity")
    body = record["body"]
    if type(body) is not dict or hashed(body) != record["hash"]:
        raise ValueError("body digest")
    if record["status"] == "probe_error":
        if set(body) != {"stage", "detail_hash"} or type(body["stage"]) is not str or body["stage"] not in STAGES or not is_hash(body["detail_hash"]):
            raise ValueError("failure attribution")
        return record
    if type(record["status"]) is not str or record["status"] not in {"pass", "difference", "observed", "unqualified"} or set(body) != BODY_FIELDS:
        raise ValueError("body/status schema")
    if type(body["exercised"]) is not bool or not nullable_bool(body["config_preserved"]):
        raise ValueError("control flags")
    calls = body["handler_calls"]
    if type(calls) is not list or len(calls) != 2 or any(type(count) is not int or not 0 <= count <= 1000 for count in calls):
        raise ValueError("handler attribution")
    telemetry(body["request_telemetry"])
    telemetry(body["retry_telemetry"])
    if case["kind"] == "mutation":
        if body["config_preserved"] is not None or body["request_telemetry"] is not None or calls != [0, 0] or not body["exercised"]:
            raise ValueError("mutation attribution")
    else:
        if type(body["config_preserved"]) is not bool:
            raise ValueError("budget preservation attribution")
        if body["request_telemetry"] is None or body["retry_telemetry"] is None:
            raise ValueError("execution telemetry missing")
        handler = case.get("cancel") in {"handler-value", "handler-error", "error-value-only", "deadline-handler"}
        if body["exercised"] != (not handler or calls[0] > 0 and calls[1] > 0) or (not handler and calls != [0, 0]):
            raise ValueError("handler exercise attribution")
    events = body["events"]
    if type(events) is not list or not 1 <= len(events) <= 100:
        raise ValueError("event count")
    observed_layout = []
    for event in events:
        if type(event) is not dict or set(event) != EVENT_FIELDS or not is_hash(event["hash"]):
            raise ValueError("event fields")
        if type(event["id"]) is not str or type(event["phase"]) is not str or type(event["class"]) is not str:
            raise ValueError("event classification types")
        if event["phase"] == "request":
            if event["class"] not in REQUEST_CLASSES:
                raise ValueError("request classification")
        elif event["phase"] == "api_error":
            if event["class"] != "io_error" or event["matches"] is not False:
                raise ValueError("API error classification")
        elif event["phase"] not in {"baseline", "cache_before", "cache_after", "after_eval", "retry"} or event["class"] not in VALUE_CLASSES:
            raise ValueError("value classification")
        if any(not nullable_bool(event[key]) for key in ["matches", "present", "cache_matches_expected", "inspection_agrees"]):
            raise ValueError("event flags")
        if event["staleness"] is not None and (type(event["staleness"]) is not str or event["staleness"] not in {"Current", "Dirty", "NeverEvaluated"}):
            raise ValueError("freshness classification")
        if event["phase"] == "cache_before" and event["matches"] is not None:
            raise ValueError("cache read promoted to verification")
        if event["phase"] not in {"request", "api_error"} and (type(event["present"]) is not bool or event["staleness"] is None or type(event["inspection_agrees"]) is not bool):
            raise ValueError("cache metadata")
        if event["phase"] in {"baseline", "after_eval", "retry"}:
            if type(event["matches"]) is not bool:
                raise ValueError("result verification missing")
            if event["matches"] and (event["present"] is not True or event["staleness"] != "Current" or event["cache_matches_expected"] is not True or event["inspection_agrees"] is not True):
                raise ValueError("unverified result promoted")
        if event["phase"] == "request" and case.get("expected") != "observe" and type(event["matches"]) is not bool:
            raise ValueError("request verification missing")
        if event["phase"] == "cache_after" and case.get("expected") != "observe" and type(event["matches"]) is not bool:
            raise ValueError("request cache verification missing")
        if event["phase"] == "request" and event["matches"] is True:
            expected = case["expected"]
            valid = expected == "success" and (event["class"] == "preparation_success" if case["route"] == "prepare" else event["class"].startswith("success:"))
            valid |= expected == "resource" and event["class"] == "resource:" + str(case["reason"])
            valid |= expected == "cancelled" and event["class"] == "cancelled"
            valid |= expected == "cell_error" and event["class"] == "success:cell_error:" + str(case["reason"])
            if not valid:
                raise ValueError("request expectation binding")
        if event["phase"] == "cache_after" and event["matches"] is True:
            current = case["expected"] in {"success", "cell_error"} and case["route"] != "prepare"
            if (event["staleness"] == "Current") != current or event["inspection_agrees"] is not True or current and (event["present"] is not True or event["cache_matches_expected"] is not True):
                raise ValueError("request result currentness binding")
        if event["phase"] not in {"request", "api_error"} and event["cache_matches_expected"] is True:
            bind_match(event, event_expected(case, manifest, event["id"], event["phase"]))
        observed_layout.append((event["id"], event["phase"]))
    layout = expected_layout(case, manifest)
    if not any(event["phase"] == "api_error" for event in events):
        if observed_layout != layout:
            raise ValueError("event ordering/coverage")
    else:
        # Retain bounded, attributed API failures while still requiring the
        # unaffected cache/result observations in their declared order.
        allowed = set(layout) | {(identity, "api_error") for identity, _ in layout}
        if case["kind"] == "mutation":
            model = next(model for model in manifest["models"] if model["id"] == case["model_id"])
            allowed |= {(action["id"], "api_error") for action in model["actions"]}
        if len(set(observed_layout)) != len(observed_layout) or any(item not in allowed for item in observed_layout):
            raise ValueError("API failure coverage")
        cursor = 0
        action_errors = {item for item in allowed if item[1] == "api_error" and not any(item[0] == identity for identity, _ in layout)}
        for identity, phase in observed_layout:
            if (identity, phase) in action_errors:
                continue
            if cursor >= len(layout) or layout[cursor][0] != identity or not (layout[cursor][1] == phase or phase == "api_error" and layout[cursor][1] in {"baseline", "after_eval", "retry"}):
                raise ValueError("API failure ordering")
            cursor += 1
        if cursor != len(layout):
            raise ValueError("API failure completeness")
    expected_status = "difference" if any(event["matches"] is False for event in events) or body["config_preserved"] is False else "unqualified" if not body["exercised"] else "observed" if case.get("expected") == "observe" else "pass"
    if record["status"] != expected_status:
        raise ValueError("aggregate status")
    return record


def fallback(case, stage, digest):
    body = {"stage": stage, "detail_hash": digest}
    return {"id": case["id"], "status": "probe_error", "class": case["kind"], "hash": hashed(body), "body": body}


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
              "cases": records, "counts": dict(Counter(record["status"] for record in records)), "scope": "Finite in-memory engine/request controls and mutation observations; not hard OS isolation or production qualification"}
    result = destination / "results.private.json"
    with result.open("x", encoding="utf-8") as out:
        json.dump(report, out, indent=2)
    print(json.dumps({"class": "complete_execution_cases", "cases": len(records), "counts": report["counts"], "hash": file_sha(result)}), flush=True)
    return 2 if any(record["status"] == "probe_error" for record in records) else 1 if any(record["status"] in {"difference", "unqualified"} for record in records) else 0


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--result-dir", type=Path, required=True)
    args = parser.parse_args()
    raise SystemExit(collect(args.binary.resolve(strict=True), args.result_dir.resolve(), load_manifest()))
