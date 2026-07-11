"""hnswlib L2 HNSW adapter -- secondary replication check for the HNSW family.

Pinned to hnswlib==0.8.0 per protocol.toml. Import-guarded like faiss_cpu.py:
importing this module always succeeds; instantiating without hnswlib
installed raises a clear RuntimeError.
"""

from __future__ import annotations

import pathlib

import numpy as np

from .base import AnnAdapter

try:
    import hnswlib

    HNSWLIB_AVAILABLE = True
    HNSWLIB_VERSION = getattr(hnswlib, "__version__", "0.8.0")
except ImportError:  # pragma: no cover - exercised only when hnswlib is absent
    hnswlib = None
    HNSWLIB_AVAILABLE = False
    HNSWLIB_VERSION = None


def _require_hnswlib() -> None:
    if not HNSWLIB_AVAILABLE:
        raise RuntimeError(
            "hnswlib is not installed. Install "
            "benchmarks/ann799/requirements-macos-arm64.txt before running "
            "the hnswlib system."
        )


class HnswlibAdapter(AnnAdapter):
    index_kind = "HNSW"
    search_param_name = "ef"

    def __init__(self, m: int = 32, ef_construction: int = 200, seed: int = 100):
        self._m = m
        self._ef_construction = ef_construction
        self._seed = seed
        self._index = None
        self._dim: int | None = None
        self._count: int = 0

    def build(self, base_vectors: np.ndarray, out_dir: pathlib.Path) -> None:
        _require_hnswlib()
        self._dim = base_vectors.shape[1]
        self._count = base_vectors.shape[0]
        self._index = hnswlib.Index(space="l2", dim=self._dim)
        self._index.init_index(
            max_elements=self._count,
            M=self._m,
            ef_construction=self._ef_construction,
            random_seed=self._seed,
        )
        self._index.set_num_threads(1)
        ids = np.arange(self._count, dtype=np.int64)
        self._index.add_items(np.ascontiguousarray(base_vectors, dtype=np.float32), ids)

    def load(self, out_dir: pathlib.Path) -> None:
        _require_hnswlib()
        meta_path = out_dir / "dim.txt"
        self._dim = int(meta_path.read_text().strip())
        self._index = hnswlib.Index(space="l2", dim=self._dim)
        self._index.load_index(str(out_dir / "index.hnswlib"))
        self._index.set_num_threads(1)

    def set_search_width(self, value: int) -> None:
        self._index.set_ef(int(value))

    def search_one(self, query_vector: np.ndarray, k: int) -> np.ndarray:
        q = np.ascontiguousarray(query_vector.reshape(1, -1), dtype=np.float32)
        ids, _ = self._index.knn_query(q, k=k)
        return ids[0].astype(np.int64, copy=False)

    def save(self, out_dir: pathlib.Path) -> None:
        out_dir.mkdir(parents=True, exist_ok=True)
        self._index.save_index(str(out_dir / "index.hnswlib"))
        (out_dir / "dim.txt").write_text(str(self._dim))

    def artifact_paths(self, out_dir: pathlib.Path) -> list[pathlib.Path]:
        return [out_dir / "index.hnswlib", out_dir / "dim.txt"]

    def metadata(self) -> dict:
        return {
            "library": "hnswlib",
            "library_version": HNSWLIB_VERSION,
            "index_kind": self.index_kind,
            "metric": "l2",
            "threads": 1,
            "build_params": {
                "m": self._m,
                "ef_construction": self._ef_construction,
                "seed": self._seed,
            },
            "search_param_name": self.search_param_name,
        }


ADAPTERS = {"hnswlib": HnswlibAdapter}
