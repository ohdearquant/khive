use super::*;
use khive_runtime::Namespace;

#[tokio::test]
async fn resolve_project_id_rejects_like_wildcard_input() {
    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    let project = rt
        .create_entity_with_embedding_report(
            &token,
            "project",
            None,
            "WildcardIngestTest",
            None,
            None,
            vec![],
        )
        .await
        .map(|(row, _report)| row)
        .unwrap();
    let compact = project.id.simple().to_string();
    let wildcard_input = format!("{}%", &compact[..8]);

    let resolved = resolve_project_id(&rt, &wildcard_input).await.unwrap();
    assert_eq!(
        resolved, None,
        "a %-bearing project argument must not resolve via a wildcard LIKE scan"
    );
}

#[tokio::test]
async fn resolve_id_resolves_compact_prefix_over_8_chars() {
    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    let project = rt
        .create_entity_with_embedding_report(
            &token,
            "project",
            None,
            "CompactIngestTest",
            None,
            None,
            vec![],
        )
        .await
        .map(|(row, _report)| row)
        .unwrap();
    let compact = project.id.simple().to_string();

    let resolved = resolve_id(&rt, &token, &compact[..16]).await.unwrap();
    assert_eq!(resolved, Some(project.id));
}
