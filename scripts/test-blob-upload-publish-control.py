"""A copied blob publisher must fail the shared-publication construction witness.

Run in an isolated checkout; the mutation driver checks its head and clean state.
"""
import argparse
import hashlib
import json
import re
import subprocess
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--root", type=Path, required=True)
parser.add_argument("--out", type=Path, required=True)
args = parser.parse_args()
root, out = args.root.resolve(), args.out.resolve()
out.mkdir(parents=True, exist_ok=False)
target = root / "crates/khive-db/src/stores/blob_uploads.rs"
original = target.read_bytes()
source = (root / "crates/khive-db/src/stores/blob.rs").read_bytes()
anchor = b"            publish_blob_at(\n"
assert original.count(anchor) == 1, "commit call must be unique"
start = source.index(b"fn publish_blob_at(")
assert source.count(b"fn publish_blob_at(") == 1
opening = source.index(b"{", start)
depth = 1
closing = opening + 1
while depth:
    byte = source[closing]
    depth += (byte == ord("{")) - (byte == ord("}"))
    closing += 1
copy = source[start:closing].replace(b"fn publish_blob_at(", b"fn publish_upload_copy(", 1)
mutant = original.replace(anchor, b"            publish_upload_copy(\n", 1)
mutant += b"\n#[cfg(unix)]\n" + copy + b"\n"
head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
base = ["cargo", "test", "--manifest-path", "crates/Cargo.toml", "--locked", "-p", "khive-db", "--lib"]
green = base + ["stores::blob::uploads::tests", "--", "--nocapture"]
red = base + ["stores::blob::uploads::tests::upload_put_and_commit_share_the_publish_routine", "--", "--exact", "--nocapture"]
evidence = {"head": head, "original_sha256": hashlib.sha256(original).hexdigest(), "mutant_sha256": hashlib.sha256(mutant).hexdigest()}

def run(name, command):
    result = subprocess.run(command, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    (out / (name + ".log")).write_text(result.stdout)
    return {"command": command, "returncode": result.returncode, "selected": [int(n) for n in re.findall(r"running (\d+) tests?\b", result.stdout)]}, result.stdout

try:
    evidence["before"], log = run("publisher-before", green)
    assert evidence["before"]["returncode"] == 0 and evidence["before"]["selected"] == [11], log
    target.write_bytes(mutant)
    evidence["mutant"], log = run("publisher-mutant", red)
    evidence["valid_red"] = (evidence["mutant"]["returncode"] == 101 and evidence["mutant"]["selected"] == [1]
        and "test result: FAILED." in log and "left: 0" in log and "right: 1" in log)
    assert evidence["valid_red"], log
finally:
    target.write_bytes(original)
    evidence["bytes_restored"] = target.read_bytes() == original
    evidence["restored"], log = run("publisher-restored", green)
    (out / "control.json").write_text(json.dumps(evidence, indent=2) + "\n")
assert evidence["restored"]["returncode"] == 0 and evidence["restored"]["selected"] == [11], log
print(json.dumps(evidence, indent=2))
