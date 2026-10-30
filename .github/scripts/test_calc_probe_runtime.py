"""Fault controls for the diagnostic, independent of installed spreadsheet engines."""
import copy
import contextlib
import importlib.util
import io
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace as NS
import unittest
from unittest.mock import patch
import zipfile

import calc_probe_runtime as rt

spec = importlib.util.spec_from_file_location("authored_fixture", Path(__file__).with_name("linux-calc-probe.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
CASES = fixture.CASES
GOLD = {(s, a): {"value": expected, "formula": "=" + formula} for s, a, formula, _, expected, _ in CASES}
GOLD.update({(s, a): {"value": expected, "formula": None} for s, a, expected in rt.CONTROLS})


class Cell:
    def __init__(self, sheet, address, data):
        self.sheet, self.address, self.data = sheet, address, data

    def getCellAddress(self):
        col, row = rt.coordinates(self.address)
        return NS(Column=col + self.data.get("wrong_column", 0), Row=row, Sheet=rt.SHEET_INDEX[self.sheet])

    def getRangeAddress(self):
        col, row = rt.coordinates(self.address)
        return NS(StartColumn=col, EndColumn=col, StartRow=row, EndRow=row, Sheet=rt.SHEET_INDEX[self.sheet])

    def getCellByPosition(self, col, row):
        if (col, row) != (0, 0):
            raise AssertionError("relative single-cell offset must be0,0")
        return self

    def getError(self):
        return self.data.get("error", 0)

    def getType(self):
        return NS(value="FORMULA" if self.data["formula"] else "TEXT" if isinstance(self.data["value"], str) else "VALUE")

    def getFormula(self):
        return self.data["formula"] or str(self.data["value"])

    def getDataArray(self):
        if self.data.get("array_failure"):
            raise RuntimeError("injected unsupported typed-array route")
        return ((self.data["value"],),)

    def getPropertyValue(self, name):
        if self.data["formula"] is None:
            raise AssertionError("formula result property cannot type a literal")
        return 2 if isinstance(self.data["value"], str) else 1

    def getValue(self):
        return self.data["value"]

    def getString(self):
        return self.data["value"]


class Sheet:
    def __init__(self, name, data, faults, route):
        self.name, self.data, self.faults, self.route = name, data, faults, route

    def getName(self):
        return self.name

    def getCellByPosition(self, col, row):
        address = chr(65 + col) + str(row + 1)
        return self.cell(address, "position")

    def getCellRangeByName(self, address):
        return self.cell(address, "name")

    def cell(self, address, route):
        if (self.name, address, route) in self.faults or (self.name, address, "all") in self.faults:
            raise RuntimeError("injected route failure")
        return Cell(self.name, address, self.data[self.name, address])


class Document:
    def __init__(self, faults=(), data=None, enumeration_failure=False):
        self.data, self.faults = copy.deepcopy(GOLD if data is None else data), set(faults)
        self.enumeration_failure, self.stored = enumeration_failure, False

    def getSheets(self):
        return self

    def getElementNames(self):
        if self.enumeration_failure:
            raise RuntimeError("injected enumeration failure")
        return ("Inputs", "Calc")

    def getByName(self, name):
        return Sheet(name, self.data, self.faults, "name")

    def getByIndex(self, index):
        return Sheet(("Inputs", "Calc")[index], self.data, self.faults, "index")

    def enableAutomaticCalculation(self, value):
        pass

    def calculateAll(self):
        pass

    def storeAsURL(self, url, options):
        self.stored = True


def inspect(document):
    report = {"probe_errors": []}
    rt.inspect_document(document, CASES, report, lambda _: None)
    return report


def valid_receipt():
    inspected = inspect(Document())
    worker = {**inspected, "worker_finished": True, "source_sha256": "owned-source", "source_unchanged": True,
              "calculation_completed": True, "export_completed": True, "observation_mode": "cells",
              "cleanup": {"document_close": "completed", "desktop_terminate": "completed"}}
    return {"probe_errors": [], "source_sha256": "owned-source", "source_unchanged": True,
            "supervisor": {"returncode": 0, "stdio_within_bound": True, "group_absent": True},
            "server_cleanup": {"group_absent": True}, "worker": worker,
            "export": {"literal_controls_match": True, "formula_presence_preserved": True,
                       "cases": [{"case_id": case[5], "matches_expected": True} for case in CASES]}}


class DiagnosticFaultControls(unittest.TestCase):
    def test_export_only_control_never_claims_memory_observation(self):
        report = {"probe_errors": []}
        doc = Document()
        with tempfile.TemporaryDirectory() as tmp, patch.object(rt, "inspect_document", side_effect=AssertionError("inspection forbidden in control")):
            rt.execute_workbook(doc, Path(tmp) / "new.xlsx", CASES, report, lambda _: None, lambda n, v: (n, v), "export-only")
        self.assertTrue(doc.stored)
        self.assertFalse(report["in_memory_observation_complete"])
        self.assertEqual(report["observation_mode"], "export-only")
        self.assertNotIn("cases", report)

    def test_modes_cannot_substitute_for_each_other_or_hide_calculation_failure(self):
        report = valid_receipt()
        report["observation_mode"] = "export-only"
        self.assertEqual(rt.decide_report(report, "uno", CASES), 2)
        worker = report["worker"]
        worker.update(observation_mode="export-only", in_memory_observation_complete=False,
                      in_memory_observation_skipped="export-only control")
        worker.pop("cases")
        worker.pop("controls")
        self.assertEqual(rt.decide_report(report, "uno", CASES), 0)
        worker["calculation_completed"] = False
        self.assertEqual(rt.decide_report(report, "uno", CASES), 2)
        worker["calculation_completed"] = True
        report["observation_mode"] = "cells"
        self.assertEqual(rt.decide_report(report, "uno", CASES), 2)

    def test_runtime_receipt_contains_hashes_and_versions_without_local_paths(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "synthetic-module"
            path.write_bytes(b"synthetic runtime identity")
            with patch.dict(os.environ, {"CALC_CLIENT_FAMILY": "unqualified"}):
                result = rt.runtime_identity(NS(__file__=path), NS(__file__=path))
            self.assertEqual(set(result), {"python_version", "python_binary", "uno_sha256", "pyuno_sha256", "client_family"})
            self.assertEqual(len(result["uno_sha256"]), 64)
            self.assertNotIn(str(Path(tmp)), json.dumps(result))

    def test_vendor_client_rejects_fallback_or_unqualified_modules(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "synthetic-module"
            path.write_bytes(b"unexpected runtime")
            with patch.dict(os.environ, {"CALC_CLIENT_FAMILY": "tdf-bundled"}), self.assertRaises(ValueError):
                rt.runtime_identity(NS(__file__=path), NS(__file__=path))

    def test_collects_early_and_late_cell_failures_and_still_exports(self):
        doc = Document({("Inputs", "B1", "all"), ("Calc", "P1", "all")})
        report = {"probe_errors": []}
        with tempfile.TemporaryDirectory() as tmp:
            rt.execute_workbook(doc, Path(tmp) / "new.xlsx", CASES, report, lambda _: None, lambda n, v: (n, v))
        self.assertEqual(len(report["cases"]), 16)
        self.assertEqual(len(report["controls"]), 4)
        self.assertEqual(sum(r["observation_complete"] for r in report["cases"]), 14)
        self.assertTrue(doc.stored)

    def test_verified_fallback_keeps_failed_route_visible(self):
        report = inspect(Document({("Inputs", "B1", "name")}))
        self.assertTrue(report["in_memory_observation_complete"])
        self.assertTrue(report["route_failures"])
        self.assertTrue(report["cases"][0]["matches_expected"])
        self.assertEqual(report["cases"][0]["verified_routes"], 2)

    def test_enumeration_failure_does_not_hide_other_routes(self):
        report = inspect(Document(enumeration_failure=True))
        self.assertFalse(report["in_memory_observation_complete"])
        self.assertEqual(sum(r["observation_complete"] for r in report["cases"]), 16)

    def test_wrong_address_cannot_certify_correct_value(self):
        doc = Document()
        doc.data["Inputs", "B1"]["wrong_column"] = 1
        report = inspect(doc)
        self.assertFalse(report["cases"][0]["observation_complete"])

    def test_numeric_literal_fallback_does_not_read_formula_result_property(self):
        doc = Document()
        for data in doc.data.values():
            data["array_failure"] = True
        report = inspect(doc)
        self.assertTrue(report["in_memory_observation_complete"])
        self.assertTrue(all(row["matches_expected"] for row in report["cases"] + report["controls"]))

    def test_nonfinite_or_missing_typed_result_is_not_verified(self):
        for value in (None, math.nan, math.inf, True):
            with self.subTest(value_type=type(value).__name__):
                doc = Document()
                doc.data["Inputs", "B1"]["value"] = value
                self.assertFalse(inspect(doc)["cases"][0]["observation_complete"])

    def test_literalized_formula_remains_an_observed_compatibility_failure(self):
        doc = Document()
        doc.data["Inputs", "B1"]["formula"] = None
        row = inspect(doc)["cases"][0]
        self.assertTrue(row["observation_complete"])
        self.assertFalse(row["matches_expected"])

    def test_cleanup_and_receipt_failures_do_not_mask_primary_or_skip_termination(self):
        calls = []
        class Broken:
            def close(self, _):
                calls.append("close")
                raise RuntimeError("injected close failure")
            def terminate(self):
                calls.append("terminate")
                raise ValueError("injected terminate failure")
        def broken_receipt(_):
            raise OSError("injected storage failure")
        report = {"probe_errors": [{"stage": "original", "exception_type": "OriginalFailure"}]}
        rt.close_owned(Broken(), Broken(), report, broken_receipt)
        self.assertEqual(calls, ["close", "terminate"])
        self.assertEqual(report["probe_errors"][0]["stage"], "original")
        self.assertEqual({e["stage"] for e in report["probe_errors"]}, {"original", "document_close", "desktop_terminate", "cleanup_receipt"})

    def test_incomplete_or_cross_job_receipts_never_pass(self):
        changes = [lambda r: r["worker"].update(cases=[]), lambda r: r["worker"].update(controls=[]),
                   lambda r: r["worker"].update(source_sha256="different-job"),
                   lambda r: r.update(source_unchanged=False), lambda r: r["supervisor"].update(timed_out=True),
                   lambda r: r["supervisor"].update(group_absent=False),
                   lambda r: r["worker"].update(calculation_completed=False),
                   lambda r: r["worker"].update(cases=None),
                   lambda r: r["worker"].update(worker_finished=False)]
        self.assertEqual(rt.decide_report(valid_receipt(), "uno", CASES), 0)
        for change in changes:
            report = valid_receipt()
            change(report)
            self.assertEqual(rt.decide_report(report, "uno", CASES), 2)

    def test_compatibility_mismatch_is_separate_from_probe_fault(self):
        report = valid_receipt()
        report["worker"]["cases"][0]["matches_expected"] = False
        report["export"]["cases"][0]["matches_expected"] = False
        self.assertEqual(rt.decide_report(report, "uno", CASES), 1)
        self.assertTrue(report["diagnostic_complete"])

    def test_source_part_admission_and_worker_layout(self):
        with tempfile.TemporaryDirectory(prefix="calc-research-") as tmp:
            root = Path(tmp).resolve()
            source = fixture.generate(root / "input")
            output = root / "output"
            output.mkdir()
            target = output / source.name
            rt.source_admission(source, target, fixture.QUALIFIED_PART_SHA256)
            target.write_bytes(b"existing")
            with self.assertRaises(ValueError):
                rt.source_admission(source, target, fixture.QUALIFIED_PART_SHA256)

    def test_export_rejects_erased_formulas_wrong_types_and_negative_shared_index(self):
        model = {(s, a): {"kind": "str" if isinstance(v, str) else "n", "value": v, "has_formula": f is not None, "has_formula_element": f is not None} for (s, a), (v, f) in ((key, (value["value"], value["formula"])) for key, value in GOLD.items())}
        model["Inputs", "B1"]["has_formula"] = False
        model["Calc", "A1"]["kind"] = "b"
        model["Inputs", "A1"]["kind"] = "e"
        result = rt.compare_export(model, CASES)
        self.assertFalse(result["formula_presence_preserved"])
        self.assertFalse(result["cases"][1]["matches_expected"])
        self.assertFalse(result["literal_controls_match"])
        with tempfile.TemporaryDirectory() as tmp:
            path = fixture.generate(Path(tmp))
            with zipfile.ZipFile(path) as archive:
                parts = {name: archive.read(name) for name in archive.namelist()}
            parts["xl/sharedStrings.xml"] = ('<sst xmlns="' + fixture.S + '"><si><t>9</t></si></sst>').encode()
            original_fragment = b'<c r="A1" t="inlineStr"><is><t>9</t></is></c>'
            self.assertIn(original_fragment, parts["xl/worksheets/sheet1.xml"])
            parts["xl/worksheets/sheet1.xml"] = parts["xl/worksheets/sheet1.xml"].replace(original_fragment, b'<c r="A1" t="s"><v>-1</v></c>')
            invalid = Path(tmp) / "negative-index.xlsx"
            with zipfile.ZipFile(invalid, "x") as archive:
                for name, content in parts.items():
                    archive.writestr(name, content)
            with self.assertRaises(ValueError):
                rt.read_model(invalid)

    def test_empty_formula_is_not_counted_as_preserved(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = fixture.generate(Path(tmp))
            with zipfile.ZipFile(path) as archive:
                parts = {name: archive.read(name) for name in archive.namelist()}
            self.assertIn(b'<f>A1*2</f>', parts["xl/worksheets/sheet1.xml"])
            parts["xl/worksheets/sheet1.xml"] = parts["xl/worksheets/sheet1.xml"].replace(b'<f>A1*2</f>', b'<f/>')
            candidate = Path(tmp) / "empty-formula.xlsx"
            with zipfile.ZipFile(candidate, "x") as archive:
                for name, content in parts.items():
                    archive.writestr(name, content)
            self.assertFalse(rt.read_model(candidate)["Inputs", "B1"]["has_formula"])

    def test_literal_controls_reject_unknown_types_and_even_empty_formula_elements(self):
        with tempfile.TemporaryDirectory() as tmp:
            model = rt.read_model(fixture.generate(Path(tmp)))
        for raw in ({"kind": "unknown", "value": "9", "has_formula": False, "has_formula_element": False},
                    {"kind": "inlineStr", "value": "9", "has_formula": False, "has_formula_element": True}):
            mutated = copy.deepcopy(model)
            mutated["Inputs", "A1"] = raw
            self.assertFalse(rt.compare_export(mutated, CASES)["literal_controls_match"])

    def test_group_cleanup_does_not_assume_exited_leader_means_no_children(self):
        state = {"alive": True}
        def killpg(pgid, action):
            if not state["alive"]:
                raise ProcessLookupError()
            if action:
                state["alive"] = False
        with patch.object(os, "killpg", killpg, create=True), patch.object(os, "getpgrp", lambda: 999, create=True), patch.object(rt.signal, "SIGKILL", 9, create=True):
            result = rt.reap_group(NS(pid=123456, poll=lambda: 0))
        self.assertTrue(result["forced_cleanup"])
        self.assertTrue(result["group_absent"])

    def test_malformed_worker_receipt_does_not_hide_existing_export(self):
        clean_process = {"returncode": 0, "stdio_within_bound": True, "group_absent": True}
        def synthetic_worker(arguments, *_):
            source, destination = Path(arguments[-2]), Path(arguments[-1])
            destination.write_bytes(source.read_bytes())
            (destination.parent / "worker-receipt.json").write_text("[]", encoding="utf-8")
            return dict(clean_process)
        stdout = io.StringIO()
        with patch.object(rt, "os", NS(name="posix", environ={})), patch.object(rt.subprocess, "check_output", return_value="synthetic-engine"), patch.object(rt.subprocess, "Popen", return_value=NS()), patch.object(rt, "reap_group", return_value={"group_absent": True}), patch.object(rt, "supervise", side_effect=synthetic_worker), contextlib.redirect_stdout(stdout):
            status = rt.run("uno", fixture.generate, CASES, fixture.QUALIFIED_PART_SHA256)
        report = json.loads(stdout.getvalue())
        self.assertEqual(status, 2)
        self.assertIn("worker_receipt", {error["stage"] for error in report["probe_errors"]})
        self.assertEqual(len(report["export"]["cases"]), 16)

    def test_partial_receipt_survives_and_malformed_receipt_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "receipt.json"
            rt.checkpoint(path, {"stage": "observing", "cases": [{"case_id": "first"}]})
            self.assertEqual(rt.load_receipt(path)["stage"], "observing")
            path.write_text("[]", encoding="utf-8")
            with self.assertRaises(ValueError):
                rt.load_receipt(path)

    @unittest.skipUnless(os.name == "posix", "owned Linux process groups")
    def test_real_child_timeout_is_owned_and_reaped(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            result = rt.supervise([sys.executable, "-c", "import time;time.sleep(10)"], root / "out", root / "err", .1)
            self.assertTrue(result["timed_out"])
            self.assertTrue(result["forced_cleanup"])
            self.assertTrue(result["group_absent"])


if __name__ == "__main__":
    unittest.main()
