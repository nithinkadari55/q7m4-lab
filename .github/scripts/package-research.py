"""Select portable diagnostic artifacts and dependency notices, never user files."""
from pathlib import Path
import hashlib
import json
import os
import shutil
import subprocess
import sys

target = sys.argv[1]
suffix = ".exe" if target.endswith("windows-msvc") else ""
destination = Path("artifact")
destination.mkdir(exist_ok=False)
hashes = {}
for binary in ("umya-roundtrip-probe", "parser-portability-probe", "aspose-roundtrip-probe", "calamine-read-probe"):
    name = binary + suffix
    source = Path("target") / target / "release" / name
    shutil.copyfile(source, destination / name)
    hashes[name] = hashlib.sha256(source.read_bytes()).hexdigest()
for name in ("Cargo.lock", "dependencies.json"):
    shutil.copyfile(name, destination / name)
metadata = json.loads(Path("dependencies.json").read_text(encoding="utf-8"))
index = []
for package in metadata["packages"]:
    if not package.get("source"):
        continue
    root = Path(package["manifest_path"]).parent
    notices = [path for path in root.iterdir() if path.is_file()
               and path.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE"))]
    output = destination / "licenses" / (package["name"] + "-" + package["version"])
    output.mkdir(parents=True, exist_ok=True)
    for path in notices:
        shutil.copyfile(path, output / path.name)
    index.append({"name": package["name"], "version": package["version"],
                  "license": package.get("license"), "notices": [path.name for path in notices]})
(destination / "licenses" / "index.json").write_text(json.dumps(index, indent=2) + "\n", encoding="utf-8")
manifest = {"commit": os.environ["GITHUB_SHA"], "run_id": os.environ["GITHUB_RUN_ID"],
            "target": target, "executable_sha256": hashes,
            "cargo_lock_sha256": hashlib.sha256(Path("Cargo.lock").read_bytes()).hexdigest(),
            "rustc": subprocess.check_output(["rustc", "--version", "--verbose"], text=True),
            "note": "Selected research binaries; no workbook or runtime-results upload."}
(destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
