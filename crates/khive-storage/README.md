# khive-storage

Storage capability traits for khive's substrate: `SqlAccess`, `VectorStore`,
`TextSearch`, `GraphStore`, `NoteStore`, `EntityStore`, `EventStore`,
`SparseStore`. Zero implementations — only contracts
(ADR-005).
A concrete backend (`khive-db`'s SQLite implementation, for example) implements
these traits; the runtime and every pack depend only on this crate, never on a
specific backend.

## Capability traits

| Trait                                                      | Surface                                                                          |
| ---------------------------------------------------------- | -------------------------------------------------------------------------------- |
| `SqlAccess` / `SqlReader` / `SqlWriter`                    | pooled connections, atomic units, query planning                                 |
| `VectorStore`                                              | dense embedding insert/search/rebuild, optional filter pushdown and batch search |
| `TextSearch`                                               | FTS document upsert/search/stats, optional term-stats (IDF)                      |
| `GraphStore`                                               | edge CRUD, neighbor queries, multi-hop traversal, batched neighbor/edge fetch    |
| `NoteStore` / `EntityStore` / `EventStore`                 | substrate-specific CRUD and filtered listing                                     |
| `SparseStore`                                              | sparse (BM25-style) vector storage                                               |

Every method returns `StorageResult<T> = Result<T, StorageError>`.
`StorageError` variants (`NotFound`, `AlreadyExists`, `Conflict`,
`InvalidInput`, `Unsupported`, `Pool`, `Timeout`, `Transaction`, …) are tagged
with the offending `StorageCapability`
(`Sql | Notes | Entities | Graph | Events | Vectors | Sparse | Text`).

## Usage

Implement the trait(s) your backend supports; callers depend only on the trait object.

```rust
use async_trait::async_trait;
use khive_storage::{GraphStore, StorageResult, TraversalRequest, GraphPath};
use uuid::Uuid;

struct MyBackend;

#[async_trait]
impl GraphStore for MyBackend {
    // ... upsert_edge, upsert_edges, get_edge, get_edge_including_deleted,
    //     delete_edge, query_edges, count_edges, neighbors, purge_incident_edges

    async fn traverse(&self, request: TraversalRequest) -> StorageResult<Vec<GraphPath>> {
        // multi-hop BFS from request.roots
        Ok(Vec::new())
    }
}
```

## Capability negotiation

Optional-feature methods (`VectorStore::search_with_filter`,
`TextSearch::term_stats`, `GraphStore::get_edges` / `batch_neighbors`) ship as
default trait methods with a conservative fallback, so a minimal backend
compiles without overriding anything: `get_edges` loops `get_edge` per ID and
`batch_neighbors` loops `neighbors` per source. Backends that support batched
`IN (...)` queries or filter pushdown override the corresponding method and
the trait's `capabilities()` accessor to advertise it.

## Where this sits

Sits directly above `khive-types` and `khive-score` in the storage dependency
chain (`types -> score -> storage -> db -> query -> runtime -> pack-* -> mcp`).
`khive-db` is the sole first-party implementation (SQLite + FTS5 +
sqlite-vec); every pack and the runtime depend on the trait surface defined
here, never on `khive-db` directly.

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
