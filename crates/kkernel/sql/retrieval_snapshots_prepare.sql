-- Preparation schema for kkernel queries against the runtime-created snapshot store.
-- NOT a migration and not read by any code: it exists so scripts/lint-sql.sh can resolve
-- the table's names when it prepares the queries in this directory. The authoritative
-- definition is khive-retrieval/src/persist/core.rs; khive-pack-knowledge's vamana module
-- creates the same table. If either of those changes a column, this copy has to follow, or
-- the linter will keep passing a query the database would refuse.
CREATE TABLE IF NOT EXISTS retrieval_snapshots (
    namespace TEXT NOT NULL,
    index_type TEXT NOT NULL,
    snapshot BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (namespace, index_type)
)
