"""Reconstruct only hash-admitted authored fixtures; never read user workbooks."""
import argparse
import hashlib
from io import BytesIO
import json
from pathlib import Path
import re
import xml.etree.ElementTree as ET
import zipfile

ROOT = Path(__file__).resolve().parent
MANIFEST_HASH = "87fd87d331ac506c2f178a50b8ed8827860302bbee4aa3887e10c965dfc0751b"
HEX = re.compile(r"[0-9a-f]{64}")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def load_manifest():
    raw = (ROOT / "import-fixtures.json").read_bytes()
    if len(raw) > 1024 * 1024 or digest(raw) != MANIFEST_HASH:
        raise ValueError("manifest identity/size")
    manifest = json.loads(raw)
    if manifest["source_commit"] != "dfd2f96457ff0929114a3e6b786c2761e6db5c5d":
        raise ValueError("upstream identity")
    files = manifest["files"]
    if not files or len({file["id"] for file in files}) != len(files):
        raise ValueError("fixture identities")
    for file in files:
        name = file["filename"]
        if Path(name).name != name or not re.fullmatch(r"[a-z0-9-]+\.xlsx", name) or not HEX.fullmatch(file["sha256"]):
            raise ValueError("fixture path/identity")
        if file["date_system"] not in {"1900", "1904"} or not 1 <= len(file["parts"]) <= 64 or set(file["parts"]) != set(file["part_sha256"]):
            raise ValueError("fixture structure")
        total = 0
        for name, text in file["parts"].items():
            data = text.encode()
            total += len(data)
            if name.startswith("/") or ".." in name.split("/") or len(data) > 512 * 1024 or digest(data) != file["part_sha256"][name]:
                raise ValueError("part identity/size/path")
            if b"<!DOCTYPE" in data.upper() or b"<!ENTITY" in data.upper():
                raise ValueError("XML declarations")
            if name.endswith(".rels") and any(node.get("TargetMode", "").lower() == "external" for node in ET.fromstring(data)):
                raise ValueError("external relationship")
        if total > 2 * 1024 * 1024:
            raise ValueError("expanded fixture size")
        book = ET.fromstring(file["parts"]["xl/workbook.xml"])
        properties = book.find("{http://schemas.openxmlformats.org/spreadsheetml/2006/main}workbookPr")
        is1904 = properties is not None and properties.get("date1904") in {"1", "true"}
        if is1904 != (file["date_system"] == "1904"):
            raise ValueError("declared source epoch")
    slots = manifest["slots"]
    by_id = {file["id"]: file for file in files}
    if not slots or len({slot["id"] for slot in slots}) != len(slots):
        raise ValueError("slot identities")
    for slot in slots:
        if not re.fullmatch(r"[A-Za-z0-9_/-]+", slot["id"]) or slot["file_id"] not in by_id:
            raise ValueError("slot binding")
        if slot["policy"] not in {"native", "serial"} or slot["binding"] not in {"loader_default", "explicit_source_epoch"}:
            raise ValueError("slot treatment")
        if type(slot["row"]) is not int or type(slot["col"]) is not int or not 1 <= slot["row"] <= 256 or not 1 <= slot["col"] <= 64:
            raise ValueError("slot address")
        if slot["expected"]["kind"] not in {"number", "text", "boolean", "error", "empty"}:
            raise ValueError("expected type")
    return manifest


def container(parts):
    output = BytesIO()
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_STORED) as archive:
        for name, text in sorted(parts.items()):
            info = zipfile.ZipInfo(name, date_time=(2020, 1, 1, 0, 0, 0))
            info.create_system = 3
            info.external_attr = 0o600 << 16
            archive.writestr(info, text.encode())
    return output.getvalue()


def prepare(destination):
    manifest = load_manifest()
    destination.mkdir(exist_ok=False)
    for file in manifest["files"]:
        raw = container(file["parts"])
        if digest(raw) != file["sha256"]:
            raise ValueError("reconstructed ZIP identity")
        (destination / file["filename"]).write_bytes(raw)
    return len(manifest["files"])


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, default=ROOT / "fixture-inputs")
    args = parser.parse_args()
    print(json.dumps({"class": "synthetic_fixture_admission", "files": prepare(args.destination), "manifest_sha256": MANIFEST_HASH}))
