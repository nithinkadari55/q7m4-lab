"""Adversarial supervisor controls with mocked child processes; no Rust execution."""
import argparse
from contextlib import redirect_stdout
import importlib.util
import io
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("candidate_supervisor", ROOT / "run_probe.py")
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
CASE = "semantic-v2/00-input-precedent"
CANARY = "PRIVATE_RAW_PAYLOAD_MUST_NEVER_REACH_CALLER"
GOOD = {"id": CASE, "status": "match", "class": "Number", "hash": "a" * 64}
controls = []


def check(label, action):
    try:
        action()
    except Exception as error:
        controls.append({"id": label, "status": "failed", "class": type(error).__name__, "hash": probe.sha(str(error).encode())})
    else:
        controls.append({"id": label, "status": "passed"})


def require(condition):
    if not condition:
        raise AssertionError("control expectation")


def reject(payload):
    try:
        probe.validate_receipt(payload, CASE)
    except (ValueError, TypeError, KeyError):
        return
    raise AssertionError("malformed receipt admitted")


class Process:
    def __init__(self, running=False, exitcode=0):
        self.running = running
        self.returncode = None if running else exitcode
        self.killed = False
        self.waited = False

    def poll(self):
        return None if self.running else self.returncode

    def kill(self):
        self.killed = True
        self.running = False
        self.returncode = -9

    def wait(self, timeout):
        self.waited = True
        return self.returncode


def worker_control(payload, expected_error=None, running=False, clock=None, exitcode=0, stderr=b""):
    process = Process(running, exitcode)
    def launch(_args, **kwargs):
        kwargs["stdout"].write(payload)
        kwargs["stdout"].flush()
        kwargs["stderr"].write(stderr)
        kwargs["stderr"].flush()
        return process
    with tempfile.TemporaryDirectory(prefix="fault-worker-", dir=ROOT) as temporary:
        output = io.StringIO()
        with patch.object(probe.subprocess, "Popen", side_effect=launch), patch.object(probe.time, "monotonic", side_effect=clock or [0, 0]), redirect_stdout(output):
            record, error, digest = probe.worker(Path("mock-only-not-a-rust-binary"), 0, Path(temporary), CASE)
        require(error == expected_error)
        require(process.waited)
        require(process.killed == running)
        require(CANARY not in output.getvalue())
        require((record is not None) == (expected_error is None))
        if expected_error:
            require(probe.valid_hash(digest))


def cohort_control(mode, exitcode):
    fixture = json.loads((ROOT / "model-fixture.json").read_bytes())
    invoked = []
    with tempfile.TemporaryDirectory(prefix="fault-cohort-", dir=ROOT) as temporary:
        work = Path(temporary)
        shutil.copyfile(ROOT / "model-fixture.json", work / "model-fixture.json")
        binary = work / "mock-not-rust.bin"
        binary.write_bytes(b"test-only")
        def run(_binary, index, _work, identity):
            invoked.append(index)
            record = {**GOOD, "id": identity}
            if index == 0 and mode == "failure":
                raise subprocess.SubprocessError(CANARY)
            if index == 1 and mode == "failure":
                return None, "worker_receipt_invalid", probe.sha(CANARY.encode())
            if index == 2 and mode == "failure":
                return {**record, "id": "different-case"}, None, None
            if index == 3 and mode == "failure":
                return None, "worker_timeout", probe.sha(CANARY.encode())
            if index == 0 and mode == "mismatch":
                record["status"] = "mismatch"
            if index == 0 and mode == "temporal":
                record.update(status="representation_unqualified", **{"class": "Date", "comparison_class": "candidate_excel1900_calendar_serial_not_independently_qualified", "comparison_status": "candidate_match", "comparison_hash": "c" * 64})
            return record, None, None
        output = io.StringIO()
        with patch.object(probe, "ROOT", work), patch.object(probe, "worker", side_effect=run), patch.object(sys, "argv", ["run_probe.py", "--binary", str(binary)]), redirect_stdout(output):
            try:
                probe.main()
            except SystemExit as exit_signal:
                require(exit_signal.code == exitcode)
            else:
                raise AssertionError("aggregate exit missing")
        report = json.loads((work / "runtime-results.json").read_text())
        journal = [json.loads(line) for line in (work / "runtime-results.slots.jsonl").read_text().splitlines()]
        require(invoked == list(range(53)))
        require(len(report["slots"]) == 53 and journal == report["slots"])
        require([slot["id"] for slot in journal] == [case["id"] for case in fixture["cases"]])
        require(sum(report["counts"]["semantic-v2"].values()) == 37 and sum(report["counts"]["control-v1"].values()) == 16)
        require(CANARY not in output.getvalue() and CANARY not in json.dumps(report))
        if mode == "failure":
            require(report["counts"]["semantic-v2"]["probe_error"] == 4)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--result-file", default="supervisor-fault-controls.json")
    args = parser.parse_args()
    if Path(args.result_file).name != args.result_file:
        parser.error("new basename required")
    destination = ROOT / args.result_file
    if destination.exists():
        parser.error("preserve prior controls; choose new basename")
    check("valid-scalar", lambda: require(probe.validate_receipt(dict(GOOD), CASE) == GOOD))
    malformed = [
        ("raw-extra-field", {**GOOD, "actual": CANARY}),
        ("falsified-counts-field", {**GOOD, "counts": {"match": 53}}),
        ("missing-hash", {key: value for key, value in GOOD.items() if key != "hash"}),
        ("non-string-hash", {**GOOD, "hash": [CANARY]}),
        ("uppercase-hash", {**GOOD, "hash": "A" * 64}),
        ("raw-class", {**GOOD, "class": CANARY}),
        ("unhashable-class", {**GOOD, "class": [CANARY]}),
        ("unhashable-status", {**GOOD, "status": [CANARY]}),
        ("wrong-id", {**GOOD, "id": "different-case"}),
        ("non-string-id", {**GOOD, "id": [CASE]}),
        ("mislabeled-error-match", {**GOOD, "class": "error:#REF!"}),
        ("mislabeled-date-match", {**GOOD, "class": "Date"}),
        ("missing-temporal-fields", {**GOOD, "status": "representation_unqualified", "class": "Date"}),
        ("orphan-temporal-field", {**GOOD, "comparison_hash": None}),
    ]
    for label, record in malformed:
        check(label, lambda record=record: reject(record))
    for reason in sorted(probe.RESOURCE_REASONS):
        receipt = {**GOOD, "status": "resource_limit", "class": "resource:" + reason}
        check("resource-" + reason, lambda receipt=receipt: require(probe.validate_receipt(receipt, CASE) == receipt))
    temporal = {**GOOD, "status": "representation_unqualified", "class": "Date", "comparison_class": "candidate_excel1900_calendar_serial_not_independently_qualified", "comparison_status": "candidate_match", "comparison_hash": "b" * 64}
    check("valid-unqualified-temporal", lambda: require(probe.validate_receipt(temporal, CASE) == temporal))
    check("missing-temporal-hash", lambda: reject({key: value for key, value in temporal.items() if key != "comparison_hash"}))
    check("missing-candidate-hash", lambda: reject({**temporal, "comparison_hash": None}))
    check("raw-temporal-hash", lambda: reject({**temporal, "comparison_hash": CANARY}))
    check("unavailable-with-hash", lambda: reject({**temporal, "comparison_status": "unavailable"}))
    check("worker-valid", lambda: worker_control(json.dumps(GOOD).encode()))
    check("worker-malformed-output", lambda: worker_control(CANARY.encode(), "worker_receipt_invalid"))
    check("worker-extra-output", lambda: worker_control(json.dumps(GOOD).encode() + b"\n" + CANARY.encode(), "worker_receipt_invalid"))
    check("worker-wrong-id", lambda: worker_control(json.dumps({**GOOD, "id": "different-case"}).encode(), "worker_receipt_invalid"))
    check("worker-nonzero", lambda: worker_control(CANARY.encode(), "worker_exit", exitcode=17))
    check("worker-oversized-receipt", lambda: worker_control(b"x" * 65_537, "worker_receipt_limit"))
    check("worker-late-valid-receipt", lambda: worker_control(json.dumps(GOOD).encode(), "worker_timeout", running=True, clock=[0, 16]))
    check("worker-output-limit", lambda: worker_control(b"", "worker_output_limit", running=True, clock=[0, 0], stderr=b"x" * 1_000_001))
    for mode, exitcode in [("match", 0), ("mismatch", 1), ("temporal", 1), ("failure", 2)]:
        check("cohort-" + mode + "-exit-" + str(exitcode), lambda mode=mode, exitcode=exitcode: cohort_control(mode, exitcode))
    report = {"scope": "mocked supervisor only; zero Rust binaries compiled/executed; no workbook engine observations", "controls": controls,
              "passed": sum(item["status"] == "passed" for item in controls), "total": len(controls),
              "supervisor_sha256": probe.file_sha(ROOT / "run_probe.py"), "controls_source_sha256": probe.file_sha(Path(__file__))}
    with destination.open("x", encoding="utf-8") as output:
        json.dump(report, output, indent=2)
    print(json.dumps({key: report[key] for key in ["scope", "passed", "total"]}))
    raise SystemExit(0 if report["passed"] == report["total"] else 1)


if __name__ == "__main__":
    main()
