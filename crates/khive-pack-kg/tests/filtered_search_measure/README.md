# Filtered search measurement integrity

`filtered_search_measure.rs` retains the manual measurement introduced for
#3774. Its ignored run emits 960 samples: four corpus sizes (200, 1,000, 10,000,
100,000), two isolated scoring arms, three kind/type filter combinations, four
limits (1, 10, 50, 100), two cache states, and five samples per cell. Each corpus
has the same 100 eligible IDs and eligible document/vector inputs. The complete
manual matrix has no fill, recall, or latency threshold; raw owner-host runs
remain necessary before the issue's measurement acceptance is complete.

The ordinary `filtered_search_oracle_and_arm_counts_are_non_vacuous` test uses
40 rows with 20 eligible rows and limit 10. Its deterministic integrity checks
are separate from performance calibration:

| Scoring arm | Filter                    | Text candidates | Vector candidates | Expected returned IDs                |
| ----------- | ------------------------- | --------------: | ----------------: | ------------------------------------ |
| Text        | Kind                      |              20 |                 0 | UUID integers 1 through 10, in order |
| Text        | Type                      |              40 |                 0 | UUID integers 1 through 10, in order |
| Text        | Kind and type             |              30 |                 0 | UUID integers 1 through 10, in order |
| Vector      | Each of the three filters |               0 |                40 | UUID integers 1 through 10, in order |

These expectations come from the fixture, independently of the measured row:

- Eligible IDs are 1 through 20. Their descriptions contain the same one query
  term with strictly increasing padding. Complete FTS ranking therefore orders
  eligible IDs from shortest to longest; the oracle asserts that complete order.
- Ineligible rows have a different kind for the Kind case; all have the selected
  kind for Type; half have it for Kind and type. FTS applies the kind filter before
  its cap of 40, while entity type is checked after candidate retrieval. Every
  seeded text document matches `retrievalchannel`.
- The vector query is the first unit basis vector. Eligible document vectors
  have second coordinate `1 + id/1000`, hence strictly decreasing cosine as the
  ID increases. All 40 candidates fit the cap. No document contains the vector
  query's `vectorprobe` text, so its text arm is empty. The existing small real
  KNN comparison also checks the independent full vector oracle.
- Neither query exactly matches an entity title, so the entity-search exact-name
  bonus does not alter the single-arm order. Ten results and fill/recall of one
  are therefore fixture facts, not quality thresholds for the larger corpora.

The same six real measured rows exercise negative controls. Each row is cloned
and independently corrupted into a coherent one-result report, a same-length
ranking with the wrong eligible ID, a wrong oracle ID, and each arm count set to
one. Each corruption must fail at its named field. This detects a validator that
merely checks nonempty results, nonzero counts, or agreement with its own oracle.

Candidate counts remain **separate same-parameter probes**, not instrumentation
of the timed dispatch. The shared output helper labels the first target after
reopening a runtime and the resident state after five discarded target requests;
it explicitly makes no cold SQLite or OS-page-cache claim. Seeding, output
validation and these integrity controls remain outside timed dispatch. Preserve
those qualifications when interpreting raw JSONL.

From `crates/`, the focused integrity command is:

```sh
cargo test -p khive-pack-kg --test filtered_search_measure filtered_search_oracle_and_arm_counts_are_non_vacuous -- --exact
```

The manual ignored measurement and its existing output directory setting are
unchanged. Follow `scripts/perf/README.md` for the pre-registered plan, same-SHA
build/run evidence, exclusive host window and cache-state interpretation. This
integrity change does not supply the outstanding raw measurements for #3774.
