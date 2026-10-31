"""Native-qualified synthetic scalar corpus; not a production calculation adapter."""
import argparse
import hashlib
import json
from pathlib import Path
import sys
import zipfile

FIXTURE = Path(__file__).resolve().parents[1] / 'fixtures/semantic-calc-v2.json'
FIXTURE_SHA256 = 'ead346c85c314aaabac78db0856bd50e01de520b085aa49f84a386a2c4c8d58b'


def load_fixture():
    if FIXTURE.stat().st_size > 32768:
        raise ValueError('bounded authored fixture required')
    content = FIXTURE.read_bytes()
    if hashlib.sha256(content).hexdigest() != FIXTURE_SHA256:
        raise ValueError('qualified semantic fixture changed')
    payload = json.loads(content)
    if len(payload['cases']) != 37 or len({c[5] for c in payload['cases']}) != 37 or len(payload['parts']) != 6:
        raise ValueError('qualified fixture identity set changed')
    if {name: hashlib.sha256(value.encode('utf-8')).hexdigest() for name, value in payload['parts'].items()} != payload['part_sha256']:
        raise ValueError('qualified workbook part changed')
    return payload


PAYLOAD = load_fixture()
CASES = PAYLOAD['cases']
QUALIFIED_PART_SHA256 = PAYLOAD['part_sha256']


def generate(directory):
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / 'synthetic-calc.xlsx'
    with zipfile.ZipFile(path, 'x', compression=zipfile.ZIP_DEFLATED) as archive:
        for name, content in PAYLOAD['parts'].items():
            info = zipfile.ZipInfo(name, date_time=(2020, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, content.encode('utf-8'))
    return path


if __name__ == '__main__':
    import calc_probe_runtime as runtime

    parser = argparse.ArgumentParser()
    parser.add_argument('--generate', type=Path)
    parser.add_argument('--driver', choices=('cli', 'uno'), default='uno')
    parser.add_argument('--observation', choices=('cells', 'export-only'), default='cells')
    parser.add_argument('--uno-worker', nargs=3, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.uno_worker:
        try:
            status = runtime.uno_worker(*args.uno_worker, CASES, QUALIFIED_PART_SHA256, args.observation)
        except Exception as error:
            print(json.dumps({'stage': 'worker_admission_or_receipt', 'exception_type': type(error).__name__}), file=sys.stderr)
            status = 2
        raise SystemExit(status)
    if args.generate:
        print(generate(args.generate))
    else:
        raise SystemExit(runtime.run(args.driver, generate, CASES, QUALIFIED_PART_SHA256, args.observation))
