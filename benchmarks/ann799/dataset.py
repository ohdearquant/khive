#!/usr/bin/env python3
"""SIFT `.fvecs`/`.ivecs` readers and dataset manifest for ANN-799.

Implements the "Dataset and exact reference" section of
docs/design/adr-799-baseline-plan.md (khive-work): frozen-file SHA256s,
vector count/dimension validation, and the immutable calibration/evaluation
query split. Every adapter and the runner import this module rather than
re-parsing the binary formats themselves, so every contender sees the same
bytes.

fvecs/ivecs layout (little-endian, repeated per vector):
    int32 dim
    dim * (float32 | int32) values

CLI:
    python3 benchmarks/ann799/dataset.py manifest --data DIR --out FILE.json
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import sys

import numpy as np

FVECS_DTYPE = np.float32
IVECS_DTYPE = np.int32


def _read_xvecs(path: pathlib.Path, value_dtype: np.dtype) -> np.ndarray:
    """Read a `.fvecs`/`.ivecs` file into a dense (n, dim) array.

    All vectors in a SIFT-format file share one dimension; the per-vector
    dim prefix is validated against the first vector's dim rather than
    trusted blindly.
    """
    raw = np.fromfile(path, dtype=np.int32)
    if raw.size == 0:
        raise ValueError(f"{path}: empty file")
    dim = int(raw[0])
    if dim <= 0:
        raise ValueError(f"{path}: non-positive leading dim {dim}")
    record_len = dim + 1
    if raw.size % record_len != 0:
        raise ValueError(
            f"{path}: file size not a multiple of record length "
            f"(dim={dim}, record_len={record_len}, total_int32={raw.size})"
        )
    n = raw.size // record_len
    records = raw.reshape(n, record_len)
    dims = records[:, 0]
    if not np.all(dims == dim):
        bad = int(np.argmax(dims != dim))
        raise ValueError(
            f"{path}: inconsistent per-vector dim at row {bad} "
            f"(expected {dim}, got {int(dims[bad])})"
        )
    values = records[:, 1:]
    if value_dtype == FVECS_DTYPE:
        return values.view(np.float32).astype(np.float32, copy=False)
    return values.astype(np.int32, copy=False)


def read_fvecs(path: pathlib.Path) -> np.ndarray:
    return _read_xvecs(path, FVECS_DTYPE)


def read_ivecs(path: pathlib.Path) -> np.ndarray:
    return _read_xvecs(path, IVECS_DTYPE)


def sha256_file(path: pathlib.Path, chunk_size: int = 1 << 20) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(chunk_size), b""):
            digest.update(chunk)
    return digest.hexdigest()


def file_stat(path: pathlib.Path, value_dtype: np.dtype) -> dict:
    arr = _read_xvecs(path, value_dtype)
    return {
        "path": str(path),
        "sha256": sha256_file(path),
        "count": int(arr.shape[0]),
        "dim": int(arr.shape[1]),
        "size_bytes": path.stat().st_size,
    }


def build_manifest(data_dir: pathlib.Path) -> dict:
    base = file_stat(data_dir / "sift_base.fvecs", FVECS_DTYPE)
    query = file_stat(data_dir / "sift_query.fvecs", FVECS_DTYPE)
    gt = file_stat(data_dir / "sift_groundtruth.ivecs", IVECS_DTYPE)
    return {"sift_base.fvecs": base, "sift_query.fvecs": query, "sift_groundtruth.ivecs": gt}


def validate_manifest(
    manifest: dict,
    expected_base_count: int,
    expected_query_count: int,
    expected_dim: int,
    expected_groundtruth_k: int,
) -> list[str]:
    """Return a list of human-readable validation failures (empty = pass)."""
    failures = []
    base = manifest["sift_base.fvecs"]
    query = manifest["sift_query.fvecs"]
    gt = manifest["sift_groundtruth.ivecs"]

    if base["count"] != expected_base_count:
        failures.append(f"base count {base['count']} != expected {expected_base_count}")
    if base["dim"] != expected_dim:
        failures.append(f"base dim {base['dim']} != expected {expected_dim}")
    if query["count"] != expected_query_count:
        failures.append(f"query count {query['count']} != expected {expected_query_count}")
    if query["dim"] != expected_dim:
        failures.append(f"query dim {query['dim']} != expected {expected_dim}")
    if gt["count"] != expected_query_count:
        failures.append(f"groundtruth count {gt['count']} != expected {expected_query_count}")
    if gt["dim"] != expected_groundtruth_k:
        failures.append(f"groundtruth k {gt['dim']} != expected {expected_groundtruth_k}")
    return failures


def load_split(data_dir: pathlib.Path, calibration_end: int, evaluation_start: int):
    """Load base/query/groundtruth and slice the immutable calibration/eval splits."""
    base = read_fvecs(data_dir / "sift_base.fvecs")
    query = read_fvecs(data_dir / "sift_query.fvecs")
    gt = read_ivecs(data_dir / "sift_groundtruth.ivecs")
    calibration_queries = query[:calibration_end]
    calibration_gt = gt[:calibration_end]
    evaluation_queries = query[evaluation_start:]
    evaluation_gt = gt[evaluation_start:]
    return base, calibration_queries, calibration_gt, evaluation_queries, evaluation_gt


def _cmd_manifest(args: argparse.Namespace) -> int:
    data_dir = pathlib.Path(args.data)
    manifest = build_manifest(data_dir)
    out_path = pathlib.Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(f"wrote {out_path}", file=sys.stderr)
    for name, stat in manifest.items():
        print(f"  {name}: n={stat['count']} dim={stat['dim']} sha256={stat['sha256'][:12]}...")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    manifest_cmd = sub.add_parser("manifest", help="write dataset-manifest.json for a data dir")
    manifest_cmd.add_argument("--data", required=True, help="directory with the three SIFT files")
    manifest_cmd.add_argument("--out", required=True, help="output manifest JSON path")
    manifest_cmd.set_defaults(func=_cmd_manifest)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
