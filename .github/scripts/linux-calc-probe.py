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
import tempfile
import xml.etree.ElementTree as E
import zipfile

S = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
R = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
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


def run():
    if os.name != "posix":
        raise RuntimeError("Linux-only process-group diagnostic")
    version = subprocess.check_output(["libreoffice", "--version"], timeout=15, text=True).strip()
    with tempfile.TemporaryDirectory(prefix="calc-research-") as tmp:
        root = Path(tmp)
        source = generate(root / "input")
        initial_hash = hashlib.sha256(source.read_bytes()).hexdigest()
        output = root / "output"
        output.mkdir()
        profile = root / "profile"
        process = subprocess.Popen(["libreoffice", "-env:UserInstallation=" + profile.as_uri(),
            "--headless", "--nologo", "--nodefault", "--norestore", "--convert-to",
            "xlsx:Calc MS Excel 2007 XML", "--outdir", str(output), str(source)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
        try:
            stdout, stderr = process.communicate(timeout=120)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate(timeout=15)
            raise RuntimeError("owned conversion process timed out") from None
        result = output / source.name
        if process.returncode != 0 or not result.is_file():
            raise RuntimeError("conversion did not create the expected private candidate")
        assert hashlib.sha256(source.read_bytes()).hexdigest() == initial_hash
        observed = read_results(result)
        mismatches = []
        for sheet, cell, _, _, expected, identifier in CASES:
            actual = observed.get((sheet, cell))
            match = actual == expected if isinstance(expected, str) else isinstance(actual, (int, float)) and math.isclose(actual, expected, rel_tol=0, abs_tol=1e-10)
            if not match:
                mismatches.append(identifier)
        # Emit reviewed case identifiers only, never detailed workbook results.
        print(json.dumps({"engine": version, "case_count": len(CASES), "mismatch_case_ids": mismatches,
                          "source_unchanged": True, "detailed_artifacts_uploaded": False}))
        if mismatches:
            raise SystemExit(1)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--generate", type=Path)
    args = parser.parse_args()
    if args.generate:
        print(generate(args.generate))
    else:
        run()
