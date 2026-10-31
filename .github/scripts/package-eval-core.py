"""Package selected synthetic diagnostics and notices, never runtime results."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess


def sha(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def notices(package):
    root = Path(package["manifest_path"]).resolve().parent
    directories = [root]
    # Registry packages contain their own notices. Only Git workspace packages
    # may inherit notices from their repository root; never walk beyond Cargo.
    if package.get("source", "").startswith("git+"):
        cargo = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")).resolve()
        root.relative_to(cargo)
        for parent in list(root.parents)[:6]:
            if parent == cargo or not parent.is_relative_to(cargo):
                break
            directories.append(parent)
            if (parent / ".git").exists():
                break
    selected = {}
    for directory in directories:
        for path in sorted(directory.iterdir()):
            if (path.is_file() and not path.is_symlink()
                    and path.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE"))):
                selected.setdefault(path.name, path)
    return selected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"], required=True)
    args = parser.parse_args()
    root = Path("crates/eval-core-research")
    destination = Path("eval-artifact")
    destination.mkdir(exist_ok=False)
    suffix = ".exe" if args.target.endswith("windows-msvc") else ""
    binary = root / "target" / args.target / "release" / ("formualizer-eval-semantic-probe" + suffix)
    shutil.copyfile(binary, destination / binary.name)
    shutil.copyfile(root / "Cargo.lock", destination / "Cargo.lock")
    selected_source = ["Cargo.toml", "probe.rs", "model-fixture.json", "run_probe.py", "test_supervisor_controls.py"]
    (destination / "source").mkdir()
    for name in selected_source:
        shutil.copyfile(root / name, destination / "source" / name)
    metadata = json.loads(Path("eval-dependencies.json").read_text(encoding="utf-8"))
    license_index = []
    for package in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"], item["id"])):
        if not package.get("source"):
            continue
        selected = notices(package)
        output = destination / "licenses" / (package["name"] + "-" + package["version"])
        output.mkdir(parents=True, exist_ok=False)
        for name, source in selected.items():
            shutil.copyfile(source, output / name)
        license_index.append({"name": package["name"], "version": package["version"], "source": package["source"],
                              "license": package.get("license"), "notices": {name: sha(path) for name, path in selected.items()},
                              "notice_status": "included" if selected else "not_found_requires_review"})
    (destination / "licenses/index.json").write_text(json.dumps(license_index, indent=2) + "\n", encoding="utf-8")
    graph = {"packages": [{key: package.get(key) for key in ["id", "name", "version", "source", "license"]} for package in metadata["packages"]],
             "resolve": metadata["resolve"]}
    (destination / "dependency-graph.json").write_text(json.dumps(graph, indent=2) + "\n", encoding="utf-8")
    original = subprocess.check_output(["git", "show", "HEAD:crates/eval-core-research/probe.rs"])
    manifest = {"commit": os.environ["GITHUB_SHA"], "run_id": os.environ["GITHUB_RUN_ID"], "target": args.target,
                "formualizer_commit": "dfd2f96457ff0929114a3e6b786c2761e6db5c5d", "executable_sha256": sha(binary),
                "cargo_lock_sha256": sha(root / "Cargo.lock"), "source_sha256": {name: sha(root / name) for name in selected_source},
                "committed_probe_sha256": hashlib.sha256(original).hexdigest(),
                "compiled_probe_sha256": sha(root / "probe.rs"), "source_transformation": "cargo fmt before build; exact compiled source included",
                "rustc": subprocess.check_output(["rustc", "+1.99.1", "--version", "--verbose"], text=True),
                "scope": "Core-only synthetic evaluator; no XLSX IO or production data; no runtime-result upload",
                "notices_missing": [item["name"] + "-" + item["version"] for item in license_index if not item["notices"]]}
    (destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"class": "selected_artifact", "sha256": sha(destination / "manifest.json"), "dependency_count": len(license_index), "notices_missing": len(manifest["notices_missing"])}))


if __name__ == "__main__":
    main()
