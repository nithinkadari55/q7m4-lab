"""Bounded synthetic calculation diagnostics; not a production engine adapter."""
import hashlib
import json
import math
import os
from pathlib import Path
import posixpath
import signal
import subprocess
import sys
import tempfile
import time
import urllib.parse
import uuid
import xml.etree.ElementTree as E
import zipfile

S = "{http://schemas.openxmlformats.org/spreadsheetml/2006/main}"
R = "{http://schemas.openxmlformats.org/officeDocument/2006/relationships}"
REPORT_CAP = 96 * 1024
SHEET_INDEX = {"Inputs": 0, "Calc": 1}
CONTROLS = [("Inputs", "A1", "9"), ("Inputs", "A2", 3), ("Inputs", "B2", 5), ("Calc", "G1", 41)]


def issue(stage, error, **context):
    return {"stage": stage, "exception_type": type(error).__name__[:128], **context}


def equal_typed(actual, expected):
    if isinstance(expected, str):
        return type(actual) is str and actual == expected
    return type(expected) in (int, float) and type(actual) in (int, float) and math.isfinite(actual) and math.isclose(actual, expected, rel_tol=0, abs_tol=1e-10)


def coordinates(address):
    split = 0
    while split < len(address) and "A" <= address[split] <= "Z":
        split += 1
    if not split or not address[split:].isascii() or not address[split:].isdigit():
        raise ValueError("invalid bounded fixture coordinate")
    column = 0
    for letter in address[:split]:
        column = column * 26 + ord(letter) - 64
    row = int(address[split:])
    if not 1 <= column <= 16384 or not 1 <= row <= 1048576:
        raise ValueError("fixture coordinate outside grid")
    return column - 1, row - 1


def checkpoint(path, report):
    payload = json.dumps(report, separators=(",", ":"), allow_nan=False).encode("utf-8")
    if len(payload) > REPORT_CAP:
        raise ValueError("bounded receipt exceeded")
    temporary = path.with_suffix(".partial")
    temporary.write_bytes(payload)
    os.replace(temporary, path)


def load_receipt(path):
    if not path.exists():
        return None
    if path.stat().st_size > REPORT_CAP:
        raise ValueError("worker receipt exceeds bound")
    result = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(result, dict):
        raise ValueError("worker receipt must be an object")
    return result


def source_admission(source, destination, expected_parts):
    source, destination = Path(source), Path(destination)
    if source.is_symlink() or destination.is_symlink() or destination.exists():
        raise ValueError("synthetic source/output alias or existing destination")
    if source.parent.name != "input" or destination.parent.name != "output" or source.parent.parent.resolve() != destination.parent.parent.resolve():
        raise ValueError("owned synthetic job layout required")
    root = source.parent.parent
    if not root.name.startswith("calc-research-") or root.absolute() != root.resolve() or any(p.is_symlink() for p in (root, source.parent, destination.parent)):
        raise ValueError("fresh resolved synthetic job root required")
    if source.name != "synthetic-calc.xlsx" or destination.name != source.name or source.stat().st_size > 65536:
        raise ValueError("fixed synthetic source required")
    with zipfile.ZipFile(source) as archive:
        if len(archive.infolist()) != 6 or set(archive.namelist()) != set(expected_parts):
            raise ValueError("qualified source entry set mismatch")
        for name, expected in expected_parts.items():
            entry = archive.getinfo(name)
            if entry.flag_bits & 1 or entry.file_size > 65536:
                raise ValueError("qualified source entry bounds")
            with archive.open(entry) as stream:
                content = stream.read(65537)
            if len(content) > 65536 or hashlib.sha256(content).hexdigest() != expected:
                raise ValueError("qualified source part mismatch")
    return hashlib.sha256(source.read_bytes()).hexdigest()


def inspect_document(document, cases, report, persist):
    """Collect every route/case failure; never certify a value without identity/type."""
    report["cases"], report["controls"], report["route_failures"] = [], [], []
    worksheets = {}
    try:
        sheets = document.getSheets()
    except Exception as error:
        report.setdefault("probe_errors", []).append(issue("sheet_collection", error))
        sheets = None
    try:
        report["sheet_names_match"] = sheets is not None and tuple(sheets.getElementNames()) == ("Inputs", "Calc")
    except Exception as error:
        report.setdefault("probe_errors", []).append(issue("sheet_enumeration", error))
        report["sheet_names_match"] = False
    for name, index in SHEET_INDEX.items():
        for route in ("name", "index"):
            try:
                if sheets is None:
                    raise RuntimeError("sheet collection unavailable")
                sheet = sheets.getByName(name) if route == "name" else sheets.getByIndex(index)
                if sheet.getName() != name:
                    raise ValueError("sheet identity mismatch")
                worksheets[name, route] = sheet
            except Exception as error:
                report["route_failures"].append(issue("sheet_lookup", error, sheet=name, sheet_route=route))
    wanted = [(s, a, expected, identifier, old, True) for s, a, _, old, expected, identifier in cases]
    wanted += [(s, a, expected, "control-" + s + "-" + a, None, False) for s, a, expected in CONTROLS]
    for name, address, expected, identifier, old_cache, formula_required in wanted:
        report["stage"] = "observe_" + identifier
        persist(report)
        column, row = coordinates(address)
        successes, route_status = [], []
        for sheet_route in ("name", "index"):
            worksheet = worksheets.get((name, sheet_route))
            if worksheet is None:
                continue
            for cell_route in ("position", "name"):
                stage = "cell_access"
                try:
                    if cell_route == "position":
                        cell = worksheet.getCellByPosition(column, row)
                    else:
                        bounded_range = worksheet.getCellRangeByName(address)
                        area = bounded_range.getRangeAddress()
                        if (area.StartColumn, area.EndColumn, area.StartRow, area.EndRow, area.Sheet) != (column, column, row, row, SHEET_INDEX[name]):
                            raise ValueError("named range identity mismatch")
                        cell = bounded_range.getCellByPosition(0, 0)
                    stage = "cell_identity"
                    actual_address = cell.getCellAddress()
                    if (actual_address.Column, actual_address.Row, actual_address.Sheet) != (column, row, SHEET_INDEX[name]):
                        raise ValueError("cell identity mismatch")
                    metadata = {}
                    for method in ("getError", "getType", "getFormula"):
                        try:
                            metadata[method] = getattr(cell, method)()
                        except Exception as error:
                            report["route_failures"].append(issue(method, error, case_id=identifier, sheet_route=sheet_route, cell_route=cell_route))
                    stage = "cell_metadata_completeness"
                    if len(metadata) != 3:
                        raise ValueError("required cell metadata incomplete")
                    error_code, content_type = metadata["getError"], metadata["getType"]
                    content_type = getattr(content_type, "value", content_type)
                    is_formula = content_type in ("FORMULA", 3)
                    expression = metadata["getFormula"]
                    structure_matches = is_formula == formula_required and (not formula_required or expression.startswith("="))
                    if not formula_required:
                        structure_matches = structure_matches and content_type in (("TEXT", 2) if isinstance(expected, str) else ("VALUE", 1))
                    stage = "cell_typed_result"
                    if error_code:
                        kind, value, matches = "error", None, False
                    else:
                        try:
                            data = cell.getDataArray()
                            if len(data) != 1 or len(data[0]) != 1:
                                raise ValueError("typed result shape is not1x1")
                            value = data[0][0]
                        except Exception as array_error:
                            # Optional since6.1: actual result type, never expected type.
                            report["route_failures"].append(issue("typed_data_array", array_error, case_id=identifier, sheet_route=sheet_route, cell_route=cell_route))
                            result_type = cell.getPropertyValue("FormulaResultType2") if is_formula else 1 if content_type in ("VALUE", 1) else 2 if content_type in ("TEXT", 2) else None
                            if result_type == 1:
                                value = cell.getValue()
                            elif result_type == 2:
                                value = cell.getString()
                            else:
                                raise ValueError("typed formula result unavailable")
                        if type(value) not in (str, int, float) or (type(value) in (int, float) and not math.isfinite(value)):
                            raise ValueError("unresolved or nonfinite typed result")
                        kind = "text" if type(value) is str else "number"
                        matches = equal_typed(value, expected)
                    signature = (kind, value, error_code, is_formula, structure_matches)
                    successes.append(signature)
                    retained = old_cache is not None and equal_typed(value, old_cache)
                    classification = "kind_or_formula_changed" if not structure_matches else "matched" if matches else "cell_error" if error_code else "retained_source_cache" if retained else "nonmatching_result"
                    route_status.append({"sheet_route": sheet_route, "cell_route": cell_route, "status": "observed", "classification": classification})
                except Exception as error:
                    failure = issue(stage, error, case_id=identifier, sheet_route=sheet_route, cell_route=cell_route)
                    report["route_failures"].append(failure)
                    route_status.append({"sheet_route": sheet_route, "cell_route": cell_route, "status": "unobserved", "failed_stage": stage})
        agreement = bool(successes) and all(item == successes[0] for item in successes)
        matched = agreement and successes[0][0] != "error" and successes[0][4] and equal_typed(successes[0][1], expected)
        result = {"case_id": identifier, "observation_complete": bool(successes) and agreement,
                  "matches_expected": matched, "route_agreement": agreement,
                  "verified_routes": len(successes), "routes": route_status}
        report["cases" if formula_required else "controls"].append(result)
        persist(report)
    report["in_memory_observation_complete"] = report["sheet_names_match"] and all(row["observation_complete"] for row in report["cases"] + report["controls"])


def close_owned(document, desktop, report, persist):
    report["cleanup"] = {}
    for name, target in (("document_close", document), ("desktop_terminate", desktop)):
        try:
            if target is None:
                report["cleanup"][name] = "not_created"
            else:
                returned = target.close(True) if name == "document_close" else target.terminate()
                if name == "desktop_terminate" and returned is False:
                    raise RuntimeError("termination declined")
                report["cleanup"][name] = "completed"
        except Exception as error:
            report["cleanup"][name] = "failed"
            report["probe_errors"].append(issue(name, error))
        try:
            persist(report)
        except Exception as error:
            report["probe_errors"].append(issue("cleanup_receipt", error))


def execute_workbook(document, destination, cases, report, persist, prop, observation="cells"):
    if observation not in ("cells", "export-only"):
        raise ValueError("unknown observation mode")
    report["observation_mode"] = observation
    report["stage"] = "calculate"
    persist(report)
    try:
        document.enableAutomaticCalculation(False)
        document.calculateAll()
        report["calculation_completed"] = True
    except Exception as error:
        report["probe_errors"].append(issue("calculate", error))
    if observation == "cells":
        try:
            inspect_document(document, cases, report, persist)
        except Exception as error:
            report["probe_errors"].append(issue("inspection", error))
    else:
        report["in_memory_observation_complete"] = False
        report["in_memory_observation_skipped"] = "export-only control"
    report["stage"] = "store"
    persist(report)
    try:
        document.storeAsURL(destination.as_uri(), (prop("FilterName", "Calc MS Excel 2007 XML"), prop("Overwrite", False)))
        report["export_completed"] = destination.is_file()
    except Exception as error:
        report["probe_errors"].append(issue("store", error))


def uno_worker(connection, source, destination, cases, expected_parts, observation="cells"):
    source, destination = Path(source), Path(destination)
    receipt = destination.parent / "worker-receipt.json"
    report = {"stage": "admission", "probe_errors": [], "calculation_completed": False,
              "export_completed": False, "in_memory_observation_complete": False}
    document = desktop = None
    if receipt.exists() or receipt.is_symlink():
        return 2
    suffix = connection.removeprefix("pipe,name=synthetic_calc_")
    if len(suffix) != 32 or any(c not in "0123456789abcdef" for c in suffix):
        return 2
    before = source_admission(source, destination, expected_parts)
    def persist(value):
        try:
            checkpoint(receipt, value)
        except Exception as error:
            if not any(item["stage"] == "receipt_write" for item in value["probe_errors"]):
                value["probe_errors"].append(issue("receipt_write", error))
    persist(report)
    try:
        report["source_sha256"] = before
        report["stage"] = "bootstrap"
        persist(report)
        import uno
        import pyuno
        report["client_runtime"] = runtime_identity(uno, pyuno)
        persist(report)

        def prop(name, value):
            result = uno.createUnoStruct("com.sun.star.beans.PropertyValue")
            result.Name, result.Value = name, value
            return result

        local = uno.getComponentContext()
        resolver = local.ServiceManager.createInstanceWithContext("com.sun.star.bridge.UnoUrlResolver", local)
        deadline = time.monotonic() + 15
        while True:
            try:
                context = resolver.resolve("uno:" + connection + ";urp;StarOffice.ComponentContext")
                break
            except Exception:
                if time.monotonic() >= deadline:
                    raise RuntimeError("owned bridge readiness deadline") from None
                time.sleep(.1)
        desktop = context.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", context)
        report["stage"] = "load"
        persist(report)
        document = desktop.loadComponentFromURL(source.as_uri(), "_blank", 0, (
            prop("Hidden", True), prop("ReadOnly", False), prop("UpdateDocMode", 0),
            prop("MacroExecutionMode", 0), prop("RepairPackage", False)))
        if document is None or not document.supportsService("com.sun.star.sheet.SpreadsheetDocument") or document.getURL() != source.as_uri():
            raise ValueError("loaded document identity unverified")
        execute_workbook(document, destination, cases, report, persist, prop, observation)
        report["source_unchanged"] = hashlib.sha256(source.read_bytes()).hexdigest() == before
    except Exception as error:
        report["probe_errors"].append(issue(report["stage"], error))
    finally:
        try:
            report["source_unchanged"] = hashlib.sha256(source.read_bytes()).hexdigest() == before
        except Exception as error:
            report["source_unchanged"] = False
            report["probe_errors"].append(issue("source_verification", error))
        close_owned(document, desktop, report, persist)
        report["stage"] = "finished"
        report["worker_finished"] = True
        persist(report)
    return 0


def reap_group(process, wait_seconds=0):
    result = {"returncode": process.poll(), "forced_cleanup": False, "group_absent": False, "errors": []}
    try:
        if process.poll() is None and wait_seconds:
            try:
                process.wait(timeout=wait_seconds)
            except subprocess.TimeoutExpired:
                pass
        try:
            os.killpg(process.pid, 0)
        except ProcessLookupError:
            result["group_absent"] = True
        else:
            if process.pid <= 1 or process.pid == os.getpgrp():
                raise ValueError("refuse unowned process group")
            result["forced_cleanup"] = True
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if process.poll() is None:
            process.wait(timeout=15)
        for _ in range(20):
            try:
                os.killpg(process.pid, 0)
            except ProcessLookupError:
                result["group_absent"] = True
                break
            time.sleep(.05)
    except Exception as error:
        result["errors"].append(issue("process_group_cleanup", error))
    result["returncode"] = process.poll()
    result["scope"] = "owned_process_group_not_escaped_descendants"
    return result


def supervise(arguments, stdout_path, stderr_path, timeout):
    with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
        process = subprocess.Popen(arguments, stdout=stdout, stderr=stderr, start_new_session=True)
        result = {"timed_out": False, "forced_cleanup": False}
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            result["timed_out"] = True
        finally:
            result.update(reap_group(process))
        result["stdio_within_bound"] = stdout_path.stat().st_size <= REPORT_CAP and stderr_path.stat().st_size <= REPORT_CAP
        return result


def read_model(path):
    if path.stat().st_size > 16 * 1024 * 1024:
        raise ValueError("output compressed bound")
    parts, total = {}, 0
    with zipfile.ZipFile(path) as archive:
        if len(archive.infolist()) > 256:
            raise ValueError("output entry count")
        for entry in archive.infolist():
            if entry.filename in parts or entry.flag_bits & 1:
                raise ValueError("duplicate/encrypted output entry")
            with archive.open(entry) as stream:
                content = stream.read(min(2 * 1024 * 1024, 8 * 1024 * 1024 - total) + 1)
            total += len(content)
            if len(content) > 2 * 1024 * 1024 or total > 8 * 1024 * 1024:
                raise ValueError("actual output expansion bound")
            parts[entry.filename] = content

    def xml(name):
        content = parts[name]
        if b"<!DOCTYPE" in content.upper() or b"<!ENTITY" in content.upper():
            raise ValueError("output XML declarations unsupported")
        return E.fromstring(content)

    strings = []
    if "xl/sharedStrings.xml" in parts:
        strings = ["".join(t.text or "" for t in si.iter(S + "t")) for si in xml("xl/sharedStrings.xml").findall(S + "si")]
    book, relations = xml("xl/workbook.xml"), {}
    for relation in xml("xl/_rels/workbook.xml.rels"):
        if relation.get("Id") in relations:
            raise ValueError("duplicate relationship identity")
        relations[relation.get("Id")] = relation
    cells = {}
    for sheet in book.findall("./" + S + "sheets/" + S + "sheet"):
        relation = relations[sheet.get(R + "id")]
        target = relation.get("Target", "")
        parsed = urllib.parse.urlsplit(target)
        if relation.get("TargetMode", "Internal") != "Internal" or parsed.scheme or parsed.netloc or parsed.query or parsed.fragment or "%" in target or "\\" in target:
            raise ValueError("unsupported worksheet target")
        name = target.lstrip("/") if target.startswith("/") else posixpath.normpath("xl/" + target)
        for cell in xml(name).iter(S + "c"):
            key = sheet.get("name"), cell.get("r")
            if key in cells or len(cells) >= 16384:
                raise ValueError("duplicate cell or diagnostic cell bound")
            element, kind = cell.find(S + "v"), cell.get("t", "n")
            raw = None if element is None else element.text
            if kind == "s" and raw is not None:
                index = int(raw)
                if not 0 <= index < len(strings):
                    raise ValueError("shared string index outside collection")
                value = strings[index]
            elif kind == "n" and raw is not None:
                value = float(raw)
            elif kind == "inlineStr":
                value = "".join(t.text or "" for t in cell.iter(S + "t"))
            else:
                value = raw
            formula = cell.find(S + "f")
            valid_formula = formula is not None and bool((formula.text or "").strip()) and kind != "inlineStr"
            cells[key] = {"kind": kind, "value": value, "has_formula": valid_formula, "has_formula_element": formula is not None}
    return cells


def compare_export(model, cases):
    rows = []
    for sheet, address, _, old, expected, identifier in cases:
        cell = model.get((sheet, address))
        admitted_kinds = ("s", "str", "inlineStr") if isinstance(expected, str) else ("n",)
        matched = cell is not None and cell["kind"] in admitted_kinds and equal_typed(cell["value"], expected)
        classification = "matched" if matched else "missing_value" if cell is None else "error_token" if cell["kind"] == "e" else "retained_source_cache" if equal_typed(cell["value"], old) else "nonmatching_result"
        rows.append({"case_id": identifier, "matches_expected": matched, "classification": classification,
                     "formula_preserved": bool(cell and cell["has_formula"])})
    controls = [bool(model.get((s, a))) and model[s, a]["kind"] in (("s", "str", "inlineStr") if isinstance(expected, str) else ("n",)) and model[s, a].get("has_formula_element") is False and equal_typed(model[s, a]["value"], expected) for s, a, expected in CONTROLS]
    return {"cases": rows, "literal_controls_match": all(controls), "formula_presence_preserved": all(row["formula_preserved"] for row in rows)}


def runtime_identity(uno, pyuno):
    """Reviewed runtime identities only; no paths or workbook observations."""
    result = {"python_version": sys.version.split()[0], "python_binary": Path(sys.executable).name}
    for name, module in (("uno", uno), ("pyuno", pyuno)):
        path = Path(module.__file__).resolve()
        if not path.is_file() or path.stat().st_size > 32 * 1024 * 1024:
            raise ValueError("unexpected runtime module")
        result[name + "_sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
    family = os.environ.get("CALC_CLIENT_FAMILY", "unqualified")
    result["client_family"] = family
    if family == "tdf-bundled":
        program = Path(sys.executable).resolve().parent
        qualified = sys.version_info[:3] == (3, 13, 17) and result["python_binary"] == "python.bin"
        qualified = qualified and all(Path(module.__file__).resolve().parent == program for module in (uno, pyuno))
        qualified = qualified and result["uno_sha256"] == "b3595c91a0660ad99e85f82504b9efe1092f2dd6fa9300e27d02180a42aa4183"
        qualified = qualified and result["pyuno_sha256"] == "eb9869f1be6ccf8438cfd98d10fcb08fa365836d03145b002b956711eb73998a"
        qualified = qualified and hashlib.sha256(Path(sys.executable).read_bytes()).hexdigest() == "f452268aa72a96fa6400f38db833591bfdaea9518b079cb02b7bd10c3551f909"
        if not qualified:
            raise ValueError("vendor client differs from inspected package")
        result["vendor_client_qualified"] = True
    return result


def decide_report(report, driver, cases):
    expected_ids = {row[5] for row in cases}
    def exact(rows, ids):
        return isinstance(rows, list) and len(rows) == len(ids) and all(isinstance(row, dict) for row in rows) and {row.get("case_id") for row in rows} == ids
    process = report.get("supervisor", {})
    exported = report.get("export", {})
    valid = not report.get("probe_errors") and report.get("source_unchanged") is True and process.get("returncode") == 0 and process.get("stdio_within_bound") is True and not process.get("timed_out") and not process.get("forced_cleanup") and process.get("group_absent") is True and not process.get("errors")
    valid = valid and exact(exported.get("cases", []), expected_ids)
    matched = bool(valid) and exported.get("literal_controls_match") is True and exported.get("formula_presence_preserved") is True and all(row.get("matches_expected") is True for row in exported["cases"])
    if driver == "uno":
        worker = report.get("worker") or {}
        observation = report.get("observation_mode", "cells")
        control_ids = {"control-" + s + "-" + a for s, a, _ in CONTROLS}
        valid = valid and report.get("server_cleanup", {}).get("group_absent") is True and not report.get("server_cleanup", {}).get("forced_cleanup") and not report.get("server_cleanup", {}).get("errors")
        valid = valid and worker.get("source_sha256") == report.get("source_sha256") and worker.get("worker_finished") is True and worker.get("export_completed") is True and not worker.get("probe_errors") and worker.get("source_unchanged") is True and worker.get("calculation_completed") is True
        valid = valid and observation in ("cells", "export-only") and worker.get("observation_mode") == observation
        if observation == "cells":
            valid = valid and worker.get("in_memory_observation_complete") is True
            valid = valid and exact(worker.get("cases", []), expected_ids) and exact(worker.get("controls", []), control_ids)
            valid = valid and all(row.get("observation_complete") is True for row in worker.get("cases", []) + worker.get("controls", []))
        else:
            valid = valid and worker.get("in_memory_observation_complete") is False and worker.get("in_memory_observation_skipped") == "export-only control"
            valid = valid and not worker.get("cases") and not worker.get("controls")
        valid = valid and worker.get("cleanup") == {"document_close": "completed", "desktop_terminate": "completed"}
        matched = bool(valid and matched)
        if observation == "cells":
            matched = matched and all(row.get("matches_expected") is True for row in worker.get("cases", []) + worker.get("controls", []))
    report["diagnostic_complete"] = bool(valid)
    report["compatibility_matches"] = bool(valid and matched)
    return 2 if not valid else 0 if matched else 1


def run(driver, generate, cases, expected_parts, observation="cells"):
    if os.name != "posix":
        raise RuntimeError("Linux process supervisor required")
    if observation not in ("cells", "export-only"):
        raise ValueError("unknown observation mode")
    executable = os.environ.get("CALC_EXECUTABLE", "libreoffice")
    version = subprocess.check_output([executable, "--version"], timeout=15, text=True).strip()
    with tempfile.TemporaryDirectory(prefix="calc-research-") as tmp:
        root = Path(tmp).resolve()
        source = generate(root / "input")
        output = root / "output"
        output.mkdir()
        destination, profile = output / source.name, root / "profile"
        before = source_admission(source, destination, expected_parts)
        report = {"engine": version, "driver": driver, "source_sha256": before, "observation_mode": observation,
                  "native_qualified_source_part_bytes": True, "case_count": len(cases), "probe_errors": [],
                  "detailed_artifacts_uploaded": False}
        server = None
        try:
            arguments = [executable, "-env:UserInstallation=" + profile.as_uri(), "--headless", "--nologo", "--nodefault", "--norestore"]
            if driver == "uno":
                connection = "pipe,name=synthetic_calc_" + uuid.uuid4().hex
                server = subprocess.Popen(arguments + ["--accept=" + connection + ";urp;StarOffice.ServiceManager"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
                report["supervisor"] = supervise([sys.executable, str(Path(sys.argv[0]).resolve()), "--observation", observation, "--uno-worker", connection, str(source), str(destination)], root / "bridge.stdout", root / "bridge.stderr", 150)
                try:
                    report["worker"] = load_receipt(output / "worker-receipt.json")
                except Exception as error:
                    report["probe_errors"].append(issue("worker_receipt", error))
            else:
                report["supervisor"] = supervise(arguments + ["--convert-to", "xlsx:Calc MS Excel 2007 XML", "--outdir", str(output), str(source)], root / "cli.stdout", root / "cli.stderr", 120)
        except Exception as error:
            report["probe_errors"].append(issue("supervisor", error))
        finally:
            if server is not None:
                report["server_cleanup"] = reap_group(server, 10)
            try:
                report["source_unchanged"] = hashlib.sha256(source.read_bytes()).hexdigest() == before
            except Exception as error:
                report["probe_errors"].append(issue("source_verification", error))
                report["source_unchanged"] = False
        # Independent phase: a malformed worker receipt must not hide an export.
        if destination.is_file():
            try:
                report["export"] = compare_export(read_model(destination), cases)
            except Exception as error:
                report["probe_errors"].append(issue("export_inspection", error))
        else:
            report["probe_errors"].append({"stage": "export", "exception_type": "MissingCandidate"})
        status = decide_report(report, driver, cases)
        print(json.dumps(report, separators=(",", ":"), allow_nan=False))
        return status
