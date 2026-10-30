"""Synthetic-only external-engine diagnostic, not the production Rust backend.

Generate a tiny authored fixture locally or compare a Linux distribution's Calc
conversion against qualified expectations. No workbook or detailed runtime result
is uploaded. A passing comparison is limited to these cases and this engine build.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import uuid
import xml.etree.ElementTree as E
import zipfile

S = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
R = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
# Decompressed payloads of the SDK/native-qualified authored fixture. ZIP stream
# hashes may differ with platform/zlib; workbook part bytes must still match.
QUALIFIED_PART_SHA256 = {
    "[Content_Types].xml": "7f57845cef1cee4cc3cb1763ff4a33e0343cf583dc921466f1959051be1ca726",
    "_rels/.rels": "fc5715a86467973c771fed6cf49203ed48734dcbd0f24088be6bbb6e290cd7b5",
    "xl/workbook.xml": "044b4ca8121cc96c00ff0e08a27052c4322be5158a822599a853b74055f1e40f",
    "xl/_rels/workbook.xml.rels": "4ba410c248f448feaa0db1d20ab1026d13625101996636af24ac6d9333ca6995",
    "xl/worksheets/sheet1.xml": "a34c5b5e3767b32550287c821205167c68bad133b19c1e86f8af0e8f5ff9eaff",
    "xl/worksheets/sheet2.xml": "80daeb3ba8fcaf82bd5a63fcb46f3b0b9b4578e442469da85904047d8929e885",
}
CASES = [
    ("Inputs", "B1", "A1*2", 14, 18, "direct"),
    ("Calc", "A1", "Inputs!B1+1", 15, 19, "transitive"),
    ("Calc", "B1", "NamedInput+10", 17, 19, "defined-name"),
    ("Calc", "C1", "SUM(Inputs!A:A)", 10, 3, "whole-column-text-exclusion"),
    ("Calc", "D1", "Inputs!A1+100", 107, 109, "cross-sheet"),
    ("Calc", "E1", 'INDIRECT("\'Inputs\'!A1")*3', 21, 27, "indirect-scalar"),
    ("Calc", "F1", "2+3", 0, 5, "constant-formula"),
    ("Calc", "H1", "SUMPRODUCT(Inputs!A1:A2,Inputs!A1:A2)", 0, 9, "sumproduct-types"),
    ("Calc", "I1", "IFERROR(1/0,42)", 0, 42, "error-recovery"),
    ("Calc", "J1", "_xlfn.LET(_xlpm.x,Inputs!A1,_xlpm.x*2)", 0, 18, "let"),
    ("Calc", "K1", '_xlfn.XLOOKUP(3,Inputs!A1:A2,Inputs!B1:B2,"missing")', 0, 5, "xlookup"),
    ("Calc", "L1", "_xlfn.LAMBDA(_xlpm.x,_xlpm.x+1)(Inputs!A2)", 0, 4, "lambda-invocation"),
    ("Calc", "M1", "DATE(1900,3,1)-DATE(1900,2,28)", 0, 2, "date-1900-boundary"),
    ("Calc", "N1", 'SUM(1,"2",TRUE)', 0, 4, "scalar-coercion"),
    ("Calc", "O1", "COUNT(Inputs!A1:A2)", 0, 1, "count-text-exclusion"),
    ("Calc", "P1", '_xlfn.TEXTJOIN("-",TRUE,Inputs!A1:A2)', "old", "9-3", "textjoin"),
]
WORKER_STAGE = "not_started"


def generate(directory):
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / "synthetic-calc.xlsx"
    if path.exists():
        raise FileExistsError("synthetic destination exists")
    sheets = {}
    for name in ("Inputs", "Calc"):
        root = E.Element("worksheet", xmlns=S)
        sheets[name] = E.SubElement(root, "sheetData"), root, {}
    literals = [("Inputs", "A1", "9"), ("Inputs", "A2", 3), ("Inputs", "B2", 5), ("Calc", "G1", 41)]
    cells = [(s, a, None, v) for s, a, v in literals] + [(s, a, f, old) for s, a, f, old, _, _ in CASES]
    for name, address, formula, value in sorted(cells, key=lambda x: (x[0], int(x[1][1:]), x[1][0])):
        data, _, rows = sheets[name]
        number = address[1:]
        row = rows.get(number)
        if row is None:
            row = E.SubElement(data, "row", r=number)
            rows[number] = row
        cell = E.SubElement(row, "c", r=address)
        if formula is not None:
            if isinstance(value, str):
                cell.set("t", "str")
            E.SubElement(cell, "f").text = formula
            E.SubElement(cell, "v").text = str(value)
        elif isinstance(value, str):
            cell.set("t", "inlineStr")
            E.SubElement(E.SubElement(cell, "is"), "t").text = value
        else:
            E.SubElement(cell, "v").text = str(value)
    parts = {
        "[Content_Types].xml": '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>',
        "_rels/.rels": '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>',
        "xl/workbook.xml": '<workbook xmlns="' + S + '" xmlns:r="' + R + '"><workbookPr date1904="0"/><sheets><sheet name="Inputs" sheetId="1" r:id="rId1"/><sheet name="Calc" sheetId="2" r:id="rId2"/></sheets><definedNames><definedName name="NamedInput">Inputs!$A$1</definedName></definedNames><calcPr calcId="0" calcMode="auto" fullCalcOnLoad="1" forceFullCalc="1"/></workbook>',
        "xl/_rels/workbook.xml.rels": '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="' + R + '/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="' + R + '/worksheet" Target="worksheets/sheet2.xml"/></Relationships>',
    }
    for number, name in enumerate(("Inputs", "Calc"), 1):
        parts[f"xl/worksheets/sheet{number}.xml"] = E.tostring(sheets[name][1], encoding="utf-8", xml_declaration=True)
    with zipfile.ZipFile(path, "x", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, content in parts.items():
            info = zipfile.ZipInfo(name, date_time=(2020, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, content)
    with zipfile.ZipFile(path) as archive:
        actual = {name: hashlib.sha256(archive.read(name)).hexdigest() for name in archive.namelist()}
        if len(archive.infolist()) != 6 or actual != QUALIFIED_PART_SHA256:
            raise ValueError("authored parts differ from the native-qualified corpus")
    return path


def read_results(path):
    with zipfile.ZipFile(path) as archive:
        if len(archive.infolist()) > 256 or sum(p.file_size for p in archive.infolist()) > 8 * 1024 * 1024:
            raise ValueError("diagnostic output archive bounds")
        strings = []
        if "xl/sharedStrings.xml" in archive.namelist():
            for si in E.fromstring(archive.read("xl/sharedStrings.xml")).findall("{" + S + "}si"):
                strings.append("".join(t.text or "" for t in si.iter("{" + S + "}t")))
        book = E.fromstring(archive.read("xl/workbook.xml"))
        rels = {r.get("Id"): r.get("Target") for r in E.fromstring(archive.read("xl/_rels/workbook.xml.rels"))}
        values = {}
        for sheet in book.findall("./{" + S + "}sheets/{" + S + "}sheet"):
            target = rels[sheet.get("{" + R + "}id")]
            part = target.lstrip("/") if target.startswith("/") else "xl/" + target
            root = E.fromstring(archive.read(part))
            for cell in root.iter("{" + S + "}c"):
                element = cell.find("{" + S + "}v")
                text = None if element is None else element.text
                kind = cell.get("t", "n")
                if kind == "s" and text is not None:
                    value = strings[int(text)]
                elif kind == "n" and text is not None:
                    value = float(text)
                elif kind == "inlineStr":
                    value = "".join(t.text or "" for t in cell.iter("{" + S + "}t"))
                else:
                    value = text
                values[sheet.get("name"), cell.get("r")] = value
        return values


def uno_worker(connection, source, destination):
    global WORKER_STAGE
    WORKER_STAGE = "bootstrap"
    # Import only in the disposable Linux system-Python bridge process.
    import uno

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
                raise RuntimeError("owned UNO bridge did not become ready") from None
            time.sleep(.1)
    desktop = context.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", context)
    document = None
    try:
        WORKER_STAGE = "load"
        document = desktop.loadComponentFromURL(Path(source).as_uri(), "_blank", 0, (
            prop("Hidden", True), prop("ReadOnly", False), prop("UpdateDocMode", 0),
            prop("MacroExecutionMode", 0), prop("RepairPackage", False)))
        if document is None:
            raise RuntimeError("synthetic document load returned no component")
        document.enableAutomaticCalculation(False)
        WORKER_STAGE = "calculate"
        document.calculateAll()
        memory_mismatches = []
        for sheet, address, _, old_cache, expected, identifier in CASES:
            WORKER_STAGE = "cell_access_" + identifier
            cell = document.getSheets().getByName(sheet).getCellRangeByName(address)
            WORKER_STAGE = "cell_error_" + identifier
            if cell.getError():
                memory_mismatches.append({"case_id": identifier, "classification": "calculated_cell_error"})
            else:
                WORKER_STAGE = "cell_value_" + identifier
                actual = cell.getString() if isinstance(expected, str) else cell.getValue()
                match = actual == expected if isinstance(expected, str) else math.isclose(actual, expected, rel_tol=0, abs_tol=1e-10)
                if not match:
                    memory_mismatches.append({"case_id": identifier, "classification": "retained_source_cache" if actual == old_cache else "nonmatching_result"})
        WORKER_STAGE = "store"
        document.storeAsURL(Path(destination).as_uri(), (
            prop("FilterName", "Calc MS Excel 2007 XML"), prop("Overwrite", False)))
        print(json.dumps({"in_memory_cases": len(CASES), "in_memory_mismatches": memory_mismatches}))
    finally:
        if sys.exc_info()[0] is None:
            WORKER_STAGE = "close"
        if document is not None:
            document.close(True)
        desktop.terminate()


def convert_uno(source, destination, profile, executable):
    connection = "pipe,name=synthetic_calc_" + uuid.uuid4().hex
    server = subprocess.Popen([executable, "-env:UserInstallation=" + profile.as_uri(),
        "--headless", "--nologo", "--nodefault", "--norestore",
        "--accept=" + connection + ";urp;StarOffice.ServiceManager"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    bridge = None
    try:
        bridge = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--uno-worker",
            connection, str(source), str(destination)], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, start_new_session=True)
        try:
            stdout, stderr = bridge.communicate(timeout=150)
        except subprocess.TimeoutExpired:
            raise RuntimeError("owned UNO bridge timed out") from None
        if bridge.returncode:
            # The worker emits a controlled stage/class receipt, no values/dump.
            try:
                failure = json.loads(stderr)
            except json.JSONDecodeError:
                failure = {"stage": "unclassified", "exception_type": "unclassified"}
            raise RuntimeError("owned UNO bridge failed: " + json.dumps(failure))
        memory = json.loads(stdout)
        if memory.get("in_memory_cases") != len(CASES):
            raise RuntimeError("incomplete in-memory calculation observation")
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            raise RuntimeError("owned UNO server did not exit after terminate") from None
        return memory
    finally:
        for owned in (bridge, server):
            if owned is not None and owned.poll() is None:
                os.killpg(owned.pid, signal.SIGKILL)
                owned.wait(timeout=15)


def run(driver):
    if os.name != "posix":
        raise RuntimeError("Linux-only process-group diagnostic")
    executable = os.environ.get("CALC_EXECUTABLE", "libreoffice")
    version = subprocess.check_output([executable, "--version"], timeout=15, text=True).strip()
    with tempfile.TemporaryDirectory(prefix="calc-research-") as tmp:
        root = Path(tmp)
        source = generate(root / "input")
        initial_hash = hashlib.sha256(source.read_bytes()).hexdigest()
        output = root / "output"
        output.mkdir()
        profile = root / "profile"
        result = output / source.name
        memory = None
        if driver == "uno":
            memory = convert_uno(source, result, profile, executable)
        else:
            process = subprocess.Popen([executable, "-env:UserInstallation=" + profile.as_uri(),
                "--headless", "--nologo", "--nodefault", "--norestore", "--convert-to",
                "xlsx:Calc MS Excel 2007 XML", "--outdir", str(output), str(source)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
            try:
                stdout, stderr = process.communicate(timeout=120)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.communicate(timeout=15)
                raise RuntimeError("owned conversion process timed out") from None
            if process.returncode != 0:
                raise RuntimeError("owned conversion process failed")
        if not result.is_file():
            raise RuntimeError("conversion did not create the expected private candidate")
        assert hashlib.sha256(source.read_bytes()).hexdigest() == initial_hash
        observed = read_results(result)
        mismatches = []
        classes = []
        for sheet, cell, _, old_cache, expected, identifier in CASES:
            actual = observed.get((sheet, cell))
            match = actual == expected if isinstance(expected, str) else isinstance(actual, (int, float)) and math.isclose(actual, expected, rel_tol=0, abs_tol=1e-10)
            if not match:
                mismatches.append(identifier)
                reason = "retained_source_cache" if actual == old_cache else "missing_value" if actual is None else "error_token" if isinstance(actual, str) and actual.startswith("#") else "nonmatching_result"
                classes.append({"case_id": identifier, "classification": reason})
        # Emit reviewed case identifiers only, never detailed workbook results.
        print(json.dumps({"engine": version, "driver": driver, "source_sha256": initial_hash,
                          "case_count": len(CASES), "mismatch_case_ids": mismatches, "mismatch_classes": classes,
                          "calculated_in_memory": memory,
                          "input_text_type_retained": observed.get(("Inputs", "A1")) == "9",
                          "native_qualified_source_part_bytes": True,
                          "source_unchanged": True, "detailed_artifacts_uploaded": False}))
        if mismatches or (memory is not None and memory["in_memory_mismatches"]):
            raise SystemExit(1)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--generate", type=Path)
    parser.add_argument("--driver", choices=("cli", "uno"), default="cli")
    parser.add_argument("--uno-worker", nargs=3, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.uno_worker:
        try:
            uno_worker(*args.uno_worker)
        except Exception as error:
            print(json.dumps({"stage": WORKER_STAGE, "exception_type": type(error).__name__}), file=sys.stderr)
            raise SystemExit(1) from None
    elif args.generate:
        print(generate(args.generate))
    else:
        run(args.driver)
