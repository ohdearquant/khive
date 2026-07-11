"""Faiss CPU adapters: IndexFlatL2 (exact control), IndexHNSWFlat, IndexIVFFlat.

Pinned to faiss-cpu==1.14.3 per protocol.toml. Import-guarded: importing this
module never fails even if faiss is not installed; instantiating an adapter
without faiss installed raises a clear RuntimeError instead of an
ImportError deep in some other call stack.

Thread count is pinned to one inside `build`/`load` via
`faiss.omp_set_num_threads(1)`, matching the plan's single-thread primary
lane. Callers must also set OMP_NUM_THREADS=1 etc. in the process
environment before faiss is imported (see scripts/perf/ann799_run.sh) --
`omp_set_num_threads` alone does not guarantee every internal OpenMP region
observes it if the runtime already spun up wider thread pools.
"""

from __future__ import annotations

import pathlib

import numpy as np

from .base import AnnAdapter

try:
    import faiss

    FAISS_AVAILABLE = True
    FAISS_VERSION = getattr(faiss, "__version__", "unknown")
except ImportError:  # pragma: no cover - exercised only when faiss is absent
    faiss = None
    FAISS_AVAILABLE = False
    FAISS_VERSION = None


def _require_faiss() -> None:
    if not FAISS_AVAILABLE:
        raise RuntimeError(
            "faiss-cpu is not installed. Install "
            "benchmarks/ann799/requirements-macos-arm64.txt before running "
            "any faiss-* system."
        )


class _FaissAdapterBase(AnnAdapter):
    index_kind = "unset"

    def __init__(self) -> None:
        self._index = None
        self._dim: int | None = None

    def save(self, out_dir: pathlib.Path) -> None:
        _require_faiss()
        out_dir.mkdir(parents=True, exist_ok=True)
        faiss.write_index(self._index, str(out_dir / "index.faiss"))

    def load(self, out_dir: pathlib.Path) -> None:
        _require_faiss()
        faiss.omp_set_num_threads(1)
        self._index = faiss.read_index(str(out_dir / "index.faiss"))
        self._dim = self._index.d

    def artifact_paths(self, out_dir: pathlib.Path) -> list[pathlib.Path]:
        return [out_dir / "index.faiss"]

    def search_one(self, query_vector: np.ndarray, k: int) -> np.ndarray:
        q = np.ascontiguousarray(query_vector.reshape(1, -1), dtype=np.float32)
        _, ids = self._index.search(q, k)
        return ids[0].astype(np.int64, copy=False)

    def metadata(self) -> dict:
        return {
            "library": "faiss-cpu",
            "library_version": FAISS_VERSION,
            "index_kind": self.index_kind,
            "metric": "l2",
            "threads": 1,
        }


class FaissFlatAdapter(_FaissAdapterBase):
    """Exact vectorized control: IndexFlatL2. Not an ANN contender; no tuning."""

    index_kind = "IndexFlatL2"
    search_param_name = "none"

    def build(self, base_vectors: np.ndarray, out_dir: pathlib.Path) -> None:
        _require_faiss()
        faiss.omp_set_num_threads(1)
        self._dim = base_vectors.shape[1]
        self._index = faiss.IndexFlatL2(self._dim)
        self._index.add(np.ascontiguousarray(base_vectors, dtype=np.float32))

    def set_search_width(self, value: int) -> None:
        pass  # exact search has no width knob

    def metadata(self) -> dict:
        meta = super().metadata()
        meta["build_params"] = {}
        meta["search_param_name"] = "none"
        return meta


class FaissHNSWFlatAdapter(_FaissAdapterBase):
    """Widely deployed graph ANN reference: IndexHNSWFlat."""

    index_kind = "IndexHNSWFlat"
    search_param_name = "efSearch"

    def __init__(self, m: int = 32, ef_construction: int = 200) -> None:
        super().__init__()
        self._m = m
        self._ef_construction = ef_construction

    def build(self, base_vectors: np.ndarray, out_dir: pathlib.Path) -> None:
        _require_faiss()
        faiss.omp_set_num_threads(1)
        self._dim = base_vectors.shape[1]
        self._index = faiss.IndexHNSWFlat(self._dim, self._m)
        self._index.hnsw.efConstruction = self._ef_construction
        self._index.add(np.ascontiguousarray(base_vectors, dtype=np.float32))

    def set_search_width(self, value: int) -> None:
        self._index.hnsw.efSearch = int(value)

    def metadata(self) -> dict:
        meta = super().metadata()
        meta["build_params"] = {"m": self._m, "ef_construction": self._ef_construction}
        meta["search_param_name"] = self.search_param_name
        return meta


class FaissIVFFlatAdapter(_FaissAdapterBase):
    """Mainstream coarse-partition ANN: IndexIVFFlat, trained on first N base vectors."""

    index_kind = "IndexIVFFlat"
    search_param_name = "nprobe"

    def __init__(self, nlist: int = 4096, train_sample: int = 100_000) -> None:
        super().__init__()
        self._nlist = nlist
        self._train_sample = train_sample

    def build(self, base_vectors: np.ndarray, out_dir: pathlib.Path) -> None:
        _require_faiss()
        faiss.omp_set_num_threads(1)
        self._dim = base_vectors.shape[1]
        quantizer = faiss.IndexFlatL2(self._dim)
        self._index = faiss.IndexIVFFlat(quantizer, self._dim, self._nlist, faiss.METRIC_L2)
        train_vecs = np.ascontiguousarray(
            base_vectors[: self._train_sample], dtype=np.float32
        )
        self._index.train(train_vecs)
        self._index.add(np.ascontiguousarray(base_vectors, dtype=np.float32))

    def set_search_width(self, value: int) -> None:
        self._index.nprobe = int(value)

    def metadata(self) -> dict:
        meta = super().metadata()
        meta["build_params"] = {"nlist": self._nlist, "train_sample": self._train_sample}
        meta["search_param_name"] = self.search_param_name
        return meta


ADAPTERS = {
    "faiss-flat": FaissFlatAdapter,
    "faiss-hnswflat": FaissHNSWFlatAdapter,
    "faiss-ivfflat": FaissIVFFlatAdapter,
}
