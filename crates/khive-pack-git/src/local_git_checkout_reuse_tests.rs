use std::path::Path;
use std::sync::Arc;

use khive_runtime::blob_tree::{self as tree, TreeEntry};
use khive_runtime::KhiveRuntime;
use khive_storage::types::SqlStatement;
use khive_storage::{BlobStore, ContentRef};
use serde_json::json;

use super::reusable_tree;
use crate::receipts::{self, Disposition, Receipt};

const REPO: &str = "/repos/one";

struct Fixture {
    _directory: tempfile::TempDir,
    rt: KhiveRuntime,
    store: Arc<khive_db::stores::blob::FsBlobStore>,
}

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().expect("tempdir");
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let mut writer = rt.sql().writer().await.expect("writer");
    for sql in [
        receipts::RECEIPTS_TABLE_SQL,
        receipts::RECEIPTS_ACTOR_INDEX_SQL,
        receipts::RECEIPTS_SESSION_INDEX_SQL,
    ] {
        writer
            .execute(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: Some("git_checkout_reuse_test_schema".into()),
            })
            .await
            .expect("receipt schema");
    }
    drop(writer);
    let store = Arc::new(
        khive_db::stores::blob::FsBlobStore::new(directory.path().join("blobs"), 0)
            .expect("blob store"),
    );
    rt.install_blob_store(store.clone()).expect("install store");
    Fixture {
        _directory: directory,
        rt,
        store,
    }
}

fn commit(digit: char) -> String {
    digit.to_string().repeat(40)
}

impl Fixture {
    /// Store a tree of `files` and return (tree ref, the blob refs it names).
    async fn tree(&self, files: &[(&str, &[u8])]) -> (String, Vec<ContentRef>) {
        let mut entries = Vec::new();
        let mut blobs = Vec::new();
        for (path, bytes) in files {
            let content_ref = self.store.put(bytes.to_vec()).await.expect("put blob");
            entries.push(TreeEntry {
                path: (*path).to_string(),
                content_ref: content_ref.as_str().to_string(),
                mode: 644,
            });
            blobs.push(content_ref);
        }
        let tree_ref = tree::store(&self.rt, &entries).await.expect("store tree");
        (tree_ref, blobs)
    }

    async fn checkout_receipt(
        &self,
        namespace: &str,
        repo: &str,
        commit: &str,
        tree_ref: &str,
        disposition: Disposition,
    ) {
        let mut receipt = Receipt::new(
            namespace,
            "actor:a",
            "git.checkout",
            repo,
            json!({"ref": "HEAD"}),
            json!({"decision": "allow", "source": "git_write.allowed", "id": 0}),
            json!({"decision": "allow", "source": "policy", "id": "policy-a"}),
        );
        receipts::insert(&self.rt, &receipt).await.expect("insert");
        receipt.result = json!({"commit": commit, "tree": tree_ref, "receipt_id": receipt.id});
        receipt.disposition = disposition;
        receipt.finished_at = Some(khive_storage::now_micros());
        receipts::persist(&self.rt, &receipt).await.expect("settle");
    }

    async fn reuse(&self, namespace: &str, repo: &str, commit: &str) -> Option<String> {
        reusable_tree(&self.rt, namespace, Path::new(repo), commit).await
    }
}

#[tokio::test]
async fn reuse_answers_a_committed_checkout_of_the_same_commit_namespace_and_repository() {
    let f = fixture().await;
    let (tree_ref, _) = f.tree(&[("a", b"one"), ("d/b", b"two")]).await;
    f.checkout_receipt(
        "local",
        REPO,
        &commit('a'),
        &tree_ref,
        Disposition::Committed,
    )
    .await;

    assert_eq!(
        f.reuse("local", REPO, &commit('a')).await,
        Some(tree_ref.clone())
    );
    // The commit is matched without regard to case: a hex letter, so the case differs.
    assert_eq!(
        f.reuse("local", REPO, &commit('a').to_ascii_uppercase())
            .await,
        Some(tree_ref)
    );
    assert_eq!(f.reuse("other", REPO, &commit('a')).await, None);
    assert_eq!(f.reuse("local", "/repos/two", &commit('a')).await, None);
    assert_eq!(f.reuse("local", REPO, &commit('b')).await, None);
}

#[tokio::test]
async fn reuse_ignores_a_checkout_that_did_not_commit() {
    let f = fixture().await;
    let (tree_ref, _) = f.tree(&[("a", b"one")]).await;
    for disposition in [Disposition::NotCommitted, Disposition::Unknown] {
        f.checkout_receipt("local", REPO, &commit('1'), &tree_ref, disposition)
            .await;
    }
    assert_eq!(f.reuse("local", REPO, &commit('1')).await, None);
    f.checkout_receipt(
        "local",
        REPO,
        &commit('1'),
        &tree_ref,
        Disposition::Committed,
    )
    .await;
    assert_eq!(f.reuse("local", REPO, &commit('1')).await, Some(tree_ref));
}

#[tokio::test]
async fn reuse_refuses_a_tree_with_a_missing_blob_or_a_missing_manifest() {
    let f = fixture().await;
    let (tree_ref, blobs) = f.tree(&[("a", b"one"), ("b", b"two")]).await;
    f.checkout_receipt(
        "local",
        REPO,
        &commit('1'),
        &tree_ref,
        Disposition::Committed,
    )
    .await;
    assert_eq!(
        f.reuse("local", REPO, &commit('1')).await,
        Some(tree_ref.clone())
    );

    assert!(f.store.delete(&blobs[1]).await.expect("delete blob"));
    assert_eq!(f.reuse("local", REPO, &commit('1')).await, None);

    let (whole, _) = f.tree(&[("c", b"three")]).await;
    f.checkout_receipt("local", REPO, &commit('2'), &whole, Disposition::Committed)
        .await;
    assert_eq!(
        f.reuse("local", REPO, &commit('2')).await,
        Some(whole.clone())
    );
    let manifest = ContentRef::from_hex(&whole).expect("tree ref");
    assert!(f.store.delete(&manifest).await.expect("delete manifest"));
    assert_eq!(f.reuse("local", REPO, &commit('2')).await, None);
}

#[tokio::test]
async fn reuse_passes_over_a_broken_newer_tree_to_an_older_whole_one() {
    let f = fixture().await;
    let (older, _) = f.tree(&[("a", b"one")]).await;
    f.checkout_receipt("local", REPO, &commit('1'), &older, Disposition::Committed)
        .await;
    let (newer, blobs) = f.tree(&[("a", b"one"), ("b", b"newer only")]).await;
    f.checkout_receipt("local", REPO, &commit('1'), &newer, Disposition::Committed)
        .await;
    assert_eq!(f.reuse("local", REPO, &commit('1')).await, Some(newer));

    assert!(f.store.delete(&blobs[1]).await.expect("delete blob"));
    assert_eq!(f.reuse("local", REPO, &commit('1')).await, Some(older));
}
