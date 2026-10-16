# Pooled reader recycling

Dedicated pooled reader connections are replaced on return when either their
connection age or successful checkout count is strictly greater than its limit:

| Environment setting         | PoolConfig field | Default        |
| --------------------------- | ---------------- | -------------- |
| `KHIVE_READER_MAX_AGE_SECS` | `reader_max_age` | 300 seconds    |
| `KHIVE_READER_MAX_OPS`      | `reader_max_ops` | 5000 checkouts |

The count belongs to each physical connection and counts successful leases, not SQL
statements. With `reader_max_ops = 2`, the third return replaces the connection.
Zero replaces every successfully checked-out reader on return; zero age means
strictly positive connection age. Missing, non-Unicode or invalid unsigned values
use the defaults. Values are not trimmed or clamped. Replacements start with a new
open time and zero checkouts. Waiting, cancellation before checkout and admission
timeouts do not count as uses.

Expiry never interrupts an outstanding query. Closing and refilling happen before
the checkout's admission permit is released and its completed-hold measurement is
recorded. A failed refill retains the existing fail-closed behavior: the physical
pool loses that slot, records `reader_replacement_open_failures` and logs a warning.
`ReaderAcquisitionSnapshot::reader_discards` counts discarded returns, including
age/use expiry, failed reset/health checks and explicitly non-reusable leases. A
return that meets several discard conditions counts once, even if refill fails.

The degraded single-connection mode keeps its shared writer and database state;
it is not recycled. Explicit standalone read transactions keep their existing
transaction-age policy. The separate outstanding-checkout watchdog measures the
current hold and never resets a connection's birth time or use count.

ADR-091's early scope note predates Amendment 13. Ordinary file-backed reads now
use this pool, so recycling applies to those dedicated readers too. It is
connection hygiene, not a bound on a still-running reader or a checkpoint policy.
