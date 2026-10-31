"""Bounded diagnostic supervisor, not an approved production Python service.

Every53-case worker slot is attempted independently. Vendor stdout/stderr stay
private; only strict status/class/hash records can reach caller stdout.
"""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parent
FIELDS = {"id", "status", "class", "hash", "comparison_class", "comparison_status", "comparison_hash"}
STATUSES = {"match", "mismatch", "evaluation_error", "probe_error", "representation_unqualified", "resource_limit"}
CLASSES = {"Int", "Number", "Text", "Boolean", "Date", "DateTime", "OtherTemporal", "Array", "Empty", "Pending", "NonFinite",
           "sheet_setup", "fixture_address", "fixture_literal", "literal_setup", "fixture_name", "name_parse", "name_registration",
           "precedent_parse", "precedent_setup", "precedent_evaluation", "precedent_unobserved", "target_parse", "target_ingest", "unobserved", "panic"}
ERROR_KINDS = {"#NULL!", "#REF!", "#NAME?", "#VALUE!", "#DIV/0!", "#N/A", "#NUM!", "#ERROR!", "#N/IMPL!", "#SPILL!", "#CALC!", "#CIRC!", "#CANCELLED!"}
RESOURCE_REASONS = {"admission", "retained_memory", "scratch_memory", "work_units", "deadline", "graph_vertices", "graph_edges", "arithmetic_overflow", "materialization_cells"}
VALUE_CLASSES = {"Int", "Number", "Text", "Boolean", "Array", "Empty", "Pending", "NonFinite"}
TEMPORAL_CLASSES = {"Date", "DateTime", "OtherTemporal"}
SETUP_CLASSES = CLASSES - VALUE_CLASSES - TEMPORAL_CLASSES
COMPARISON_FIELDS = {"comparison_class", "comparison_status", "comparison_hash"}


def valid_hash(value):
    return isinstance(value, str) and len(value) == 64 and all(ch in "0123456789abcdef" for ch in value)


def validate_receipt(record, expected_id):
    if not isinstance(record, dict) or not {"id", "status", "class", "hash"} <= set(record) or not set(record) <= FIELDS:
        raise ValueError("receipt fields")
    if not isinstance(record["id"], str) or record["id"] != expected_id:
        raise ValueError("receipt identity")
    status, classification = record["status"], record["class"]
    if not isinstance(status, str) or not isinstance(classification, str) or status not in STATUSES or not valid_hash(record["hash"]):
        raise ValueError("receipt types")
    errors = {"error:" + kind for kind in ERROR_KINDS}
    resources = {"resource:" + reason for reason in RESOURCE_REASONS}
    allowed = {"match": {"Int", "Number", "Text", "Boolean"}, "mismatch": VALUE_CLASSES | errors,
               "evaluation_error": errors, "probe_error": SETUP_CLASSES,
               "representation_unqualified": TEMPORAL_CLASSES, "resource_limit": resources}
    if classification not in allowed[status]:
        raise ValueError("receipt status/class")
    if status == "representation_unqualified":
        if not COMPARISON_FIELDS <= set(record):
            raise ValueError("temporal fields")
        if record["comparison_class"] != "candidate_excel1900_calendar_serial_not_independently_qualified":
            raise ValueError("temporal class")
        comparison_status = record["comparison_status"]
        if not isinstance(comparison_status, str) or comparison_status not in {"candidate_match", "candidate_mismatch", "unavailable"}:
            raise ValueError("temporal status")
        if comparison_status == "unavailable":
            if record["comparison_hash"] is not None:
                raise ValueError("unavailable temporal hash")
        elif not valid_hash(record["comparison_hash"]):
            raise ValueError("temporal hash")
    elif COMPARISON_FIELDS & set(record):
        raise ValueError("orphan temporal fields")
    return record


def sha(payload):
    return hashlib.sha256(payload).hexdigest()


def file_sha(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def worker(binary, index, work, expected_id):
    stdout = work / f"case-{index:02d}.stdout.private"
    stderr = work / f"case-{index:02d}.stderr.private"
    started = time.monotonic()
    with stdout.open("xb") as out, stderr.open("xb") as err:
        process = subprocess.Popen([str(binary), "--case", str(index)], cwd=work, stdout=out, stderr=err)
        status = None
        try:
            while process.poll() is None:
                if time.monotonic() - started > 15:
                    status = "worker_timeout"
                    break
                if stdout.stat().st_size + stderr.stat().st_size > 1_000_000:
                    status = "worker_output_limit"
                    break
                time.sleep(0.02)
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=5)
    with stdout.open("rb") as source:
        raw = source.read(65_537)
    if status:
        return None, status, file_sha(stdout)
    if process.returncode != 0:
        return None, "worker_exit", file_sha(stdout)
    if len(raw) > 65_536:
        return None, "worker_receipt_limit", file_sha(stdout)
    try:
        record = validate_receipt(json.loads(raw), expected_id)
        return record, None, None
    except (ValueError, AssertionError, KeyError, TypeError):
        return None, "worker_receipt_invalid", sha(raw)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--result-file", default="runtime-results.json")
    args = parser.parse_args()
    if Path(args.result_file).name != args.result_file or not args.result_file.endswith(".json"):
        parser.error("new result JSON basename required")
    destination = ROOT / args.result_file
    journal = destination.with_suffix(".slots.jsonl")
    if destination.exists() or journal.exists():
        parser.error("preserve previous results; choose another result basename")
    binary = args.binary.resolve(strict=True)
    fixture_bytes = (ROOT / "model-fixture.json").read_bytes()
    fixture = json.loads(fixture_bytes)
    if sha(fixture_bytes) != "ff3e0352c6ab5be142448df56a085319323d9cf6479d53455128885423fdae9f":
        raise ValueError("fixture admission")
    if len(fixture["cases"]) != 53 or len({case["id"] for case in fixture["cases"]}) != 53:
        raise ValueError("fixture case admission")
    slots = []
    with journal.open("x", encoding="utf-8") as partial, tempfile.TemporaryDirectory(prefix="eval-workers-", dir=ROOT) as temporary:
        work = Path(temporary)
        for index, case in enumerate(fixture["cases"]):
            try:
                record, error, digest = worker(binary, index, work, case["id"])
                if record and record.get("id") != case["id"]:
                    error, digest, record = "worker_receipt_identity", sha(json.dumps(record).encode()), None
                if error:
                    record = {"id": case["id"], "status": "probe_error", "class": error, "hash": digest}
            except (OSError, subprocess.SubprocessError) as error:
                record = {"id": case["id"], "status": "probe_error", "class": "supervisor_exception", "hash": sha(type(error).__name__.encode())}
            slots.append(record)
            partial.write(json.dumps(record) + "\n")
            partial.flush()
            # This record contains no formula or expected/actual result value.
            print(json.dumps(record), flush=True)
    counts = {corpus: dict(Counter(slot["status"] for slot, case in zip(slots, fixture["cases"]) if case["corpus"] == corpus)) for corpus in ["semantic-v2", "control-v1"]}
    report = {"source_commit": fixture["source_commit"], "binary_sha256": sha(binary.read_bytes()),
              "fixture_sha256": sha(fixture_bytes), "scope": fixture["scope"], "slots": slots, "partial_evidence_journal": journal.name,
              "counts": counts, "candidate_binary_invoked": True,
              "build_provenance": "Requires separate compiler/CI/source/lock receipt; binary invocation alone does not prove origin",
              "temporal_bridge_qualification": "candidate_calendar_serial_only; not independently qualified",
              "process_rss_limit_qualified": False, "file_import_exercised": False}
    with destination.open("x", encoding="utf-8") as output:
        json.dump(report, output, indent=2)
    print(json.dumps({"class": "complete_case_slots", "status": "recorded", "hash": sha(destination.read_bytes())}))
    if any(slot["status"] == "probe_error" for slot in slots):
        raise SystemExit(2)
    if any(slot["status"] != "match" for slot in slots):
        raise SystemExit(1)
    raise SystemExit(0)


if __name__ == "__main__":
    main()
