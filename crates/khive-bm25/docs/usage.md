# BM25 Usage

## BM25 Formula

$$
\text{score}(D, Q) = \sum_{t \in Q} \text{IDF}(t) \cdot \frac{f(t, D) \cdot (k_1 + 1)}{f(t, D) + k_1 \cdot \left(1 - b + b \cdot \frac{|D|}{\text{avgdl}}\right)}
$$

| Symbol         | Meaning                      | Default |
| -------------- | ---------------------------- | ------- |
| $f(t, D)$      | Term frequency of $t$ in $D$ | —       |
| $\|D\|$        | Document length (tokens)     | —       |
| $\text{avgdl}$ | Average document length      | —       |
| $k_1$          | Term saturation              | 1.2     |
| $b$            | Length normalization         | 0.75    |

## Quick Start

```rust
use khive_bm25::{Bm25Config, Bm25Index};

let mut index = Bm25Index::try_new(Bm25Config::default()).expect("valid config");

index.index_document("doc1", "the quick brown fox").unwrap();
index.index_document("doc2", "the lazy dog").unwrap();
index.index_document("doc3", "quick brown fox jumps over the lazy dog").unwrap();

let results = index.search("quick fox", 10);
for (doc_id, score) in results {
    println!("{}: {}", doc_id, score);
}
```

## ID Types and Hybrid Search Bridging

`DocumentId` is a newtype wrapper around `String`. When performing hybrid search that combines
BM25 results with HNSW vector results (which use `NodeId` / `EmbeddingId`), bridging is
required at the call site. See the `DocumentId` struct documentation for bridging strategies.
