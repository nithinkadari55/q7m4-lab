"""Bounded real-process workbook diagnostic; no production Python service."""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import time

from prepare_fixtures import load_manifest, MANIFEST_HASH

ROOT = Path(__file__).resolve().parent
FIELDS = {"id", "status", "class", "hash", "source_sha256", "effective_epoch", "name_diagnostic_count", "name_diagnostic_hash", "cache_class", "cache_hash"}
ERRORS = {"#NULL!", "#REF!", "#NAME?", "#VALUE!", "#DIV/0!", "#N/A", "#NUM!", "#ERROR!", "#N/IMPL!", "#SPILL!", "#CALC!", "#CIRC!", "#CANCELLED!"}
VALUE_CLASSES = {"Int", "Number", "NonFinite", "Text", "Boolean", "Date", "DateTime", "Time", "Duration", "Array", "Empty", "Pending"} | {"error:" + error for error in ERRORS}
STAGES = {"fixture_path", "fixture_read", "fixture_admission", "configuration", "source_changed", "panic", "unobserved",
          "worker_timeout", "worker_output_limit", "worker_exit", "worker_receipt_limit", "worker_receipt_invalid", "supervisor_exception"}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def file_sha(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def valid_hash(value):
    return isinstance(value, str) and len(value) == 64 and all(char in "0123456789abcdef" for char in value)


def validate(record, slot, file):
    if not isinstance(record, dict) or set(record) != FIELDS:
        raise ValueError("receipt fields")
    if record["id"] != slot["id"] or type(record["id"]) is not str or record["source_sha256"] != file["sha256"]:
        raise ValueError("receipt identity")
    if not valid_hash(record["hash"]) or type(record["status"]) is not str or type(record["class"]) is not str:
        raise ValueError("receipt types")
    status, classification = record["status"], record["class"]
    errors = {"error:" + error for error in ERRORS}
    allowed = {"match": {"Int", "Number", "Text", "Boolean", "Empty"} | errors, "mismatch": VALUE_CLASSES,
               "calendar_match": {"Date", "DateTime"}, "calendar_mismatch": {"Date", "DateTime"},
               "representation_unqualified": {"Date", "DateTime", "Time", "Duration"},
               "load_error": {"source_admission", "workbook_ingest"}, "evaluation_error": errors | {"workbook_evaluation"}, "probe_error": STAGES}
    if status not in allowed or classification not in allowed[status]:
        raise ValueError("receipt status/class")
    if status == "match":
        expected = slot["expected"]
        compatible = {"number": {"Int", "Number"}, "text": {"Text"}, "boolean": {"Boolean"}, "empty": {"Empty"},
                      "error": {"error:" + str(expected["value"])}}
        if classification not in compatible[expected["kind"]]:
            raise ValueError("expected result type binding")
    if status.startswith("calendar_") and (slot["policy"] != "native" or slot.get("expected_calendar") is None):
        raise ValueError("calendar attribution")
    if status == "calendar_match":
        calendar = slot["expected_calendar"]
        if classification == "Date":
            if not calendar.endswith("T00:00:00"):
                raise ValueError("calendar time loss")
            calendar = calendar.split("T", 1)[0]
        expected_payload = json.dumps({"variant": classification, "value": calendar}, sort_keys=True, separators=(",", ":"))
        if sha(expected_payload.encode()) != record["hash"]:
            raise ValueError("calendar payload attribution")
    metadata = [record[key] for key in ["name_diagnostic_count", "name_diagnostic_hash", "cache_class", "cache_hash"]]
    if record["effective_epoch"] is None:
        if any(value is not None for value in metadata) or status not in {"load_error", "probe_error"}:
            raise ValueError("unobserved metadata")
    else:
        if record["effective_epoch"] not in {"1900", "1904"} or type(record["effective_epoch"]) is not str:
            raise ValueError("effective epoch")
        if type(metadata[0]) is not int or not 0 <= metadata[0] <= 1000 or not valid_hash(metadata[1]):
            raise ValueError("name diagnostics")
        if type(metadata[2]) is not str or metadata[2] not in VALUE_CLASSES or not valid_hash(metadata[3]) or status == "load_error":
            raise ValueError("cache attribution")
    return record


def fallback(slot, file, classification, digest):
    return {"id": slot["id"], "status": "probe_error", "class": classification, "hash": digest, "source_sha256": file["sha256"],
            "effective_epoch": None, "name_diagnostic_count": None, "name_diagnostic_hash": None, "cache_class": None, "cache_hash": None}


def worker(binary, index, fixture_dir, temporary, slot, file):
    stdout_path = temporary / (str(index) + ".stdout.private")
    stderr_path = temporary / (str(index) + ".stderr.private")
    started = time.monotonic()
    reason = None
    with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
        process = subprocess.Popen([str(binary), "--case", str(index), str(fixture_dir)], cwd=temporary, stdout=stdout, stderr=stderr)
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
        return fallback(slot, file, reason, file_sha(stdout_path))
    if process.returncode != 0:
        return fallback(slot, file, "worker_exit", file_sha(stdout_path))
    with stdout_path.open("rb") as stream:
        raw = stream.read(65_537)
    if len(raw) > 65_536:
        return fallback(slot, file, "worker_receipt_limit", file_sha(stdout_path))
    try:
        return validate(json.loads(raw), slot, file)
    except (ValueError, TypeError, KeyError):
        return fallback(slot, file, "worker_receipt_invalid", sha(raw))


def collect(binary, fixture_dir, destination, manifest):
    destination.mkdir(exist_ok=False)
    by_id = {file["id"]: file for file in manifest["files"]}
    records = []
    before = {file["id"]: file_sha(fixture_dir / file["filename"]) for file in manifest["files"]}
    if any(before[file["id"]] != file["sha256"] for file in manifest["files"]):
        raise ValueError("fixture admission before any worker")
    with (destination / "slots.private.jsonl").open("x", encoding="utf-8") as journal, tempfile.TemporaryDirectory(prefix="workers-", dir=destination) as directory:
        temporary = Path(directory)
        for index, slot in enumerate(manifest["slots"]):
            file = by_id[slot["file_id"]]
            try:
                record = validate(worker(binary, index, fixture_dir, temporary, slot, file), slot, file)
            except (ValueError, TypeError, KeyError, OSError, subprocess.SubprocessError) as error:
                record = fallback(slot, file, "supervisor_exception", sha(type(error).__name__.encode()))
            journal.write(json.dumps(record) + "\n")
            journal.flush()
            records.append(record)
            print(json.dumps(record), flush=True)
    unchanged = all(file_sha(fixture_dir / file["filename"]) == before[file["id"]] for file in manifest["files"])
    groups = {}
    for slot, record in zip(manifest["slots"], records):
        group = "/".join([slot["file_id"], slot["binding"], slot["policy"]])
        groups.setdefault(group, Counter())[record["status"]] += 1
    report = {"manifest_sha256": MANIFEST_HASH, "source_commit": manifest["source_commit"], "binary_sha256": file_sha(binary),
              "source_unchanged": unchanged, "groups": {key: dict(value) for key, value in groups.items()}, "slots": records,
              "scope": "Actual native XLSX loading without formula/name rewriting; not save/export or hard OS isolation",
              "metadata_epoch_disagreements": sum(record["effective_epoch"] is not None and record["effective_epoch"] != by_id[slot["file_id"]]["date_system"] for slot, record in zip(manifest["slots"], records))}
    result = destination / "results.private.json"
    with result.open("x", encoding="utf-8") as out:
        json.dump(report, out, indent=2)
    print(json.dumps({"class": "complete_case_slots", "status": "recorded", "slots": len(records), "source_unchanged": unchanged, "hash": file_sha(result)}), flush=True)
    if not unchanged or any(record["status"] in {"probe_error", "load_error"} for record in records):
        return 2
    return 0 if all(record["status"] in {"match", "calendar_match"} for record in records) else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--fixture-dir", type=Path, default=ROOT / "fixture-inputs")
    parser.add_argument("--result-dir", type=Path, required=True)
    args = parser.parse_args()
    raise SystemExit(collect(args.binary.resolve(strict=True), args.fixture_dir.resolve(strict=True), args.result_dir.resolve(), load_manifest()))
