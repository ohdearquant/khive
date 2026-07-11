"""Shared adapter ABI for the ANN-799 harness.

Every contender adapter (faiss_cpu, hnswlib_adapter, and the deferred
khive_vamana / diskann_memory adapters) implements this same shape so
runner.py can drive them identically. Mirrors the ABI fixed by
docs/design/adr-799-baseline-plan.md (khive-work), "Required harness work":
`build`, `load`, `set_search_width`, `search_one`, `save`, `artifact_paths`,
`metadata`.

All vectors are float32 row-major; returned IDs are integer positions in the
original SIFT base-vector row numbering (0-indexed).
"""

from __future__ import annotations

import abc
import pathlib

import numpy as np


class AnnAdapter(abc.ABC):
    """Common interface every ANN-799 contender adapter must implement."""

    #: Name of the query-time knob this adapter tunes during calibration,
    #: e.g. "efSearch", "nprobe", "ef". Must match protocol.toml's
    #: `search_param_name` for this system, or "none" for exact search.
    search_param_name: str = "none"

    @abc.abstractmethod
    def build(self, base_vectors: np.ndarray, out_dir: pathlib.Path) -> None:
        """Build the index from scratch into a clean `out_dir` and hold it in memory."""

    @abc.abstractmethod
    def load(self, out_dir: pathlib.Path) -> None:
        """Load a previously built index from `out_dir` into memory."""

    @abc.abstractmethod
    def set_search_width(self, value: int) -> None:
        """Set the query-time search-width knob (efSearch/nprobe/ef/beam/L_search)."""

    @abc.abstractmethod
    def search_one(self, query_vector: np.ndarray, k: int) -> np.ndarray:
        """Return the top-`k` result IDs (int64 array, length k) for one query vector."""

    @abc.abstractmethod
    def save(self, out_dir: pathlib.Path) -> None:
        """Persist the built index to `out_dir` so `artifact_paths` is accurate."""

    @abc.abstractmethod
    def artifact_paths(self, out_dir: pathlib.Path) -> list[pathlib.Path]:
        """Return the files that make up the durable on-disk index in `out_dir`."""

    @abc.abstractmethod
    def metadata(self) -> dict:
        """Return system/library/library_version/index_kind/build_params, etc."""


def index_bytes(paths: list[pathlib.Path]) -> int:
    return sum(p.stat().st_size for p in paths if p.is_file())
