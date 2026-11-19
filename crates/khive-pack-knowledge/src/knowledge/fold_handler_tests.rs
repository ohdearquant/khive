use super::*;

#[tokio::test]
async fn fold_skips_candidates_with_zero_live_members() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize");
    let response = KnowledgeHandlers::fold(
        &runtime,
        &token,
        json!({
            "candidates": [
                {"id": "empty", "score": 1.0, "size": 0, "members": 0},
                {"id": "live", "score": 0.8, "size": 4, "members": 1},
                {"id": "legacy", "score": 0.7, "size": 0}
            ],
            "budget": 4
        }),
    )
    .await
    .expect("fold accepts live member counts");
    let selected: Vec<_> = response["selected"]
        .as_array()
        .expect("selected candidates")
        .iter()
        .map(|item| item["id"].as_str().expect("candidate id"))
        .collect();
    assert_eq!(selected, ["live", "legacy"], "got: {response}");
    assert_eq!(response["selected_count"], 2, "got: {response}");
    assert_eq!(response["total_size"], 4, "got: {response}");
}
