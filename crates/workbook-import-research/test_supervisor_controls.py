"""Fixture/protocol/process fault controls; these do not run a Rust engine."""
import argparse
from contextlib import redirect_stdout
from copy import deepcopy
import io
import json
from pathlib import Path
import subprocess
import tempfile
from unittest.mock import patch

import prepare_fixtures as fixtures
import run_probe as probe

ROOT = Path(__file__).resolve().parent
MANIFEST = fixtures.load_manifest()
SLOT = MANIFEST["slots"][0]
FILE = next(file for file in MANIFEST["files"] if file["id"] == SLOT["file_id"])
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


def good(slot=SLOT, file=FILE):
    expected = slot["expected"]
    classification = {"number": "Number", "text": "Text", "boolean": "Boolean", "empty": "Empty", "error": "error:" + str(expected["value"])}[expected["kind"]]
    return {"id": slot["id"], "status": "match", "class": classification, "hash": "a" * 64,
            "source_sha256": file["sha256"], "effective_epoch": file["date_system"],
            "name_diagnostic_count": 0, "name_diagnostic_hash": "b" * 64, "cache_class": "Empty", "cache_hash": "c" * 64}


def reject(record, slot=SLOT, file=FILE):
    try:
        probe.validate(record, slot, file)
    except (ValueError, TypeError, KeyError):
        return
    raise AssertionError("malformed receipt admitted")


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
        require(args[1:3] == ["--case", "0"] and "--private-diagnostic" not in args)
        kwargs["stdout"].write(payload)
        kwargs["stdout"].flush()
        kwargs["stderr"].write(stderr)
        kwargs["stderr"].flush()
        return process
    with tempfile.TemporaryDirectory(prefix="import-fault-") as directory:
        output = io.StringIO()
        with patch.object(probe.subprocess, "Popen", side_effect=launch), patch.object(probe.time, "monotonic", side_effect=clock or [0, 0]), redirect_stdout(output):
            record = probe.worker(Path("mock-not-native"), 0, Path(directory), Path(directory), SLOT, FILE)
        require(process.waited and process.killed == running and CANARY not in output.getvalue())
        require(record == good() if reason is None else record["status"] == "probe_error" and record["class"] == reason)
        probe.validate(record, SLOT, FILE)


def cohort_control(mode, expected_exit):
    invoked = []
    with tempfile.TemporaryDirectory(prefix="import-cohort-") as directory:
        work = Path(directory)
        inputs = work / "inputs"
        fixtures.prepare(inputs)
        binary = work / "mock-not-rust.bin"
        binary.write_bytes(b"diagnostic control only")
        files = {file["id"]: file for file in MANIFEST["files"]}
        def worker(_binary, index, _inputs, _temporary, slot, file):
            invoked.append(index)
            record = good(slot, file)
            if index == 0:
                if mode == "exception":
                    raise subprocess.SubprocessError(CANARY)
                if mode == "invalid":
                    return {**record, "actual": CANARY}
                if mode == "mismatch":
                    record["status"] = "mismatch"
                if mode == "unqualified":
                    record.update(status="representation_unqualified", **{"class": "Date"})
                if mode == "load_error":
                    record = probe.fallback(slot, file, "unobserved", probe.sha(CANARY.encode()))
                    record.update(status="load_error", **{"class": "workbook_ingest"})
                if mode == "changed":
                    with (inputs / file["filename"]).open("ab") as out:
                        out.write(b"changed")
            return record
        output = io.StringIO()
        with patch.object(probe, "worker", side_effect=worker), redirect_stdout(output):
            code = probe.collect(binary, inputs, work / "results", MANIFEST)
        report = json.loads((work / "results/results.private.json").read_text())
        journal = [json.loads(line) for line in (work / "results/slots.private.jsonl").read_text().splitlines()]
        require(code == expected_exit and invoked == list(range(394)))
        require(report["slots"] == journal and len(journal) == 394 and len(report["groups"]) == 16)
        require([row["id"] for row in journal] == [slot["id"] for slot in MANIFEST["slots"]])
        require(sum(sum(counts.values()) for counts in report["groups"].values()) == 394)
        require(CANARY not in output.getvalue() and CANARY not in json.dumps(report))
        require(report["source_unchanged"] == (mode != "changed"))
        for slot, record in zip(MANIFEST["slots"], journal):
            probe.validate(record, slot, files[slot["file_id"]])


def fixture_control(mode):
    with tempfile.TemporaryDirectory(prefix="import-fixture-") as directory:
        work = Path(directory)
        manifest = deepcopy(MANIFEST)
        if mode == "identity":
            (work / "import-fixtures.json").write_text("{}")
            with patch.object(fixtures, "ROOT", work):
                try:
                    fixtures.load_manifest()
                except ValueError:
                    return
            raise AssertionError("foreign manifest admitted")
        if mode == "reconstruction":
            require(fixtures.prepare(work / "inputs") == 6)
            for file in manifest["files"]:
                require(probe.file_sha(work / "inputs" / file["filename"]) == file["sha256"])
            try:
                fixtures.prepare(work / "inputs")
            except FileExistsError:
                return
            raise AssertionError("prior evidence overwritten")
        file = manifest["files"][0]
        if mode == "path":
            file["filename"] = "../foreign.xlsx"
        elif mode == "epoch":
            file["date_system"] = "1904"
        elif mode == "part_hash":
            file["parts"]["xl/workbook.xml"] += " "
        elif mode == "slot_address":
            manifest["slots"][0]["row"] = True
        elif mode == "slot_duplicate":
            manifest["slots"][1]["id"] = manifest["slots"][0]["id"]
        elif mode == "external":
            key = "_rels/.rels"
            file["parts"][key] = '<Relationships><Relationship TargetMode="External" Target="https://invalid.example/"/></Relationships>'
            file["part_sha256"][key] = fixtures.digest(file["parts"][key].encode())
        raw = json.dumps(manifest).encode()
        (work / "import-fixtures.json").write_bytes(raw)
        # Fault controls admit a deliberately re-hashed test manifest to exercise
        # the structural checks beneath the immutable production manifest hash.
        with patch.object(fixtures, "ROOT", work), patch.object(fixtures, "MANIFEST_HASH", fixtures.digest(raw)):
            try:
                fixtures.load_manifest()
            except ValueError:
                return
        raise AssertionError("structural fault admitted")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--result-file", type=Path, required=True)
    args = parser.parse_args()
    check("valid-number", lambda: require(probe.validate(good(), SLOT, FILE) == good()))
    mutations = {"raw-extra": {"actual": CANARY}, "wrong-id": {"id": "wrong"}, "wrong-source": {"source_sha256": "d" * 64},
                 "uppercase-hash": {"hash": "A" * 64}, "raw-hash": {"hash": CANARY}, "bool-hash": {"hash": True},
                 "raw-class": {"class": CANARY}, "list-class": {"class": []}, "list-status": {"status": []},
                 "false-boolean-match": {"class": "Boolean"}, "false-error-match": {"class": "error:#VALUE!"},
                 "false-native-date-match": {"class": "Date"}, "bool-diagnostic-count": {"name_diagnostic_count": True},
                 "negative-diagnostic-count": {"name_diagnostic_count": -1}, "unbounded-diagnostic-count": {"name_diagnostic_count": 1001},
                 "raw-diagnostic": {"name_diagnostic_hash": CANARY}, "bad-epoch": {"effective_epoch": "gregorian"},
                 "null-epoch-with-metadata": {"effective_epoch": None}, "raw-cache": {"cache_class": CANARY},
                 "raw-cache-hash": {"cache_hash": CANARY}, "load-error-with-metadata": {"status": "load_error", "class": "workbook_ingest"}}
    for identity, mutation in mutations.items():
        check(identity, lambda mutation=mutation: reject({**good(), **mutation}))
    check("missing-field", lambda: reject({key: value for key, value in good().items() if key != "cache_hash"}))
    for expected_kind, value, classification in [("text", "hello", "Text"), ("boolean", True, "Boolean"), ("error", "#NUM!", "error:#NUM!"), ("empty", None, "Empty")]:
        slot = {**SLOT, "expected": {"kind": expected_kind, "value": value}}
        check("valid-" + expected_kind, lambda slot=slot: require(probe.validate(good(slot), slot, FILE) == good(slot)))
    for identity, slot, record in [
        ("unqualified-serial", {**SLOT, "policy": "serial"}, {**good(), "status": "representation_unqualified", "class": "DateTime"}),
        ("calendar-without-binding", SLOT, {**good(), "status": "calendar_match", "class": "DateTime"}),
        ("calendar-wrong-hash", {**SLOT, "expected_calendar": "2024-02-29T12:34:56"}, {**good(), "status": "calendar_match", "class": "DateTime"})]:
        check(identity, (lambda slot=slot, record=record: require(probe.validate(record, slot, FILE) == record)) if identity.startswith("unqualified") else (lambda slot=slot, record=record: reject(record, slot)))
    calendar_slot = {**SLOT, "expected_calendar": "2024-02-29T12:34:56"}
    calendar = {**good(), "status": "calendar_match", "class": "DateTime", "hash": probe.sha(b'{"value":"2024-02-29T12:34:56","variant":"DateTime"}')}
    check("calendar-qualified-payload", lambda: require(probe.validate(calendar, calendar_slot, FILE) == calendar))
    check("calendar-serial-mislabeled", lambda: reject(calendar, {**calendar_slot, "policy": "serial"}))
    check("calendar-lost-time", lambda: reject({**calendar, "class": "Date"}, calendar_slot))
    payload = json.dumps(good()).encode()
    for identity, arguments in [
        ("worker-valid", {}), ("worker-raw", {"payload": CANARY.encode(), "reason": "worker_receipt_invalid"}),
        ("worker-extra", {"payload": payload + CANARY.encode(), "reason": "worker_receipt_invalid"}),
        ("worker-nonzero", {"reason": "worker_exit", "code": 17}),
        ("worker-oversize-receipt", {"payload": b"x" * 65_537, "reason": "worker_receipt_limit"}),
        ("worker-late-running", {"running": True, "clock": [0, 16], "reason": "worker_timeout"}),
        ("worker-late-exited", {"clock": [0, 16], "reason": "worker_timeout"}),
        ("worker-output-running", {"running": True, "clock": [0, 0], "stderr": b"x" * 1_000_001, "reason": "worker_output_limit"}),
        ("worker-output-exited", {"stderr": b"x" * 1_000_001, "reason": "worker_output_limit"})]:
        check(identity, lambda arguments=arguments: worker_control(**{"payload": payload, **arguments}))
    for mode, exitcode in [("match", 0), ("mismatch", 1), ("unqualified", 1), ("exception", 2), ("invalid", 2), ("load_error", 2), ("changed", 2)]:
        check("cohort-" + mode, lambda mode=mode, exitcode=exitcode: cohort_control(mode, exitcode))
    for mode in ["identity", "reconstruction", "path", "epoch", "part_hash", "slot_address", "slot_duplicate", "external"]:
        check("fixture-" + mode, lambda mode=mode: fixture_control(mode))
    report = {"scope": "Mocked process, strict receipt and authored fixture controls only; zero Rust engine observations",
              "total": len(CONTROLS), "passed": sum(control["status"] == "passed" for control in CONTROLS), "controls": CONTROLS,
              "supervisor_sha256": probe.file_sha(ROOT / "run_probe.py"), "controls_source_sha256": probe.file_sha(Path(__file__))}
    with args.result_file.open("x", encoding="utf-8") as out:
        json.dump(report, out, indent=2)
    print(json.dumps({key: report[key] for key in ["scope", "total", "passed"]}))
    raise SystemExit(0 if report["total"] == report["passed"] else 1)


if __name__ == "__main__":
    main()
