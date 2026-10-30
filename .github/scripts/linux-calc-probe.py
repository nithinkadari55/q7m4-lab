"""Synthetic-only external-engine diagnostic, not the production Rust backend.

Generate a tiny authored fixture locally or compare a Linux distribution's Calc
conversion against qualified expectations. No workbook or detailed runtime result
is uploaded. A passing comparison is limited to these cases and this engine build.
"""
import argparse
import hashlib
import json
from pathlib import Path
import sys
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


if __name__ == "__main__":
    import calc_probe_runtime as runtime

    parser = argparse.ArgumentParser()
    parser.add_argument("--generate", type=Path)
    parser.add_argument("--driver", choices=("cli", "uno"), default="cli")
    parser.add_argument("--observation", choices=("cells", "export-only"), default="cells")
    parser.add_argument("--uno-worker", nargs=3, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.uno_worker:
        try:
            status = runtime.uno_worker(*args.uno_worker, CASES, QUALIFIED_PART_SHA256, args.observation)
        except Exception as error:
            print(json.dumps({"stage": "worker_admission_or_receipt", "exception_type": type(error).__name__}), file=sys.stderr)
            status = 2
        raise SystemExit(status)
    if args.generate:
        print(generate(args.generate))
    else:
        raise SystemExit(runtime.run(args.driver, generate, CASES, QUALIFIED_PART_SHA256, args.observation))
