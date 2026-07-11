# Deferred adapters

These two contenders from `protocol.toml` have no adapter module in this
slice. Neither is stubbed out with fake-passing code; `runner.py` skips any
system name with no entry in `ADAPTER_REGISTRY` and says so on stderr.

## `khive-vamana` (Rust)

**FOLLOW-UP.** The system-under-test adapter, exposing the existing
`khive-vamana` Rust index through the shared ABI in `adapters/base.py`, is
held for a follow-up PR to keep this slice's verification lane free of
Cargo (see the parent PR body). It compiles on macOS ARM; there is no
platform blocker. Implementing it requires either:

- a thin PyO3/`cffi` binding so Python can call the existing
  `VamanaGraph` build/search API directly, or
- a small standalone Rust binary (`adapters/khive_vamana` crate) that
  speaks a line-oriented protocol (`build`, `load`, `set_search_width`,
  `search_one`, `save`, `artifact_paths`, `metadata`) over stdio, driven
  from Python the same way the other adapters are.

Either shape must satisfy `AnnAdapter` in `adapters/base.py` and the fixed
construction settings (`R=64`, `Lbuild=128`, `alpha=1.0`, batch 1024) from
`docs/design/adr-799-baseline-plan.md` (khive-work), "Comparator set" and
"Operating-point and statistical protocol" sections.

## `diskann-memory` (C++)

**Optional-attempt, per the platform ruling in the parent PR.** Microsoft
DiskANN's legacy `cpp_main` C++ line at commit
`78256bbab4685e1774e78d331e081a153be26823` is not required to build on
Apple silicon. If a future run attempts it: build within a short timebox;
on failure, record `"excluded: does not build on the test platform"` in the
run's summary and manifest rather than substituting a container, a
different branch, or DiskANN3. See `protocol.toml`'s
`[systems.diskann-memory]` (`required = false`) and the plan's "Stop rather
than improvise" clause.
