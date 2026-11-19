use super::*;

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn brain_auto_feedback_accepts_presented_recall_results() {
    use khive_runtime::presentation::{
        prepare_format_value, present, OutputFormat, PresentationMode,
    };

    let (pack, rt) = make_pack();
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(khive_pack_memory::MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt.clone()),
    ));
    let registry = builder.build().expect("memory registry");
    let remembered = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "Cobalt recall forwarding preserves feedback identity",
                "memory_type": "semantic",
                "salience": 0.9
            }),
        )
        .await
        .expect("remember fixture");
    let canonical = registry
        .dispatch(
            "memory.recall",
            json!({"query": "Cobalt recall forwarding", "limit": 1, "min_score": 0.0}),
        )
        .await
        .expect("recall fixture");
    let results = prepare_format_value(
        present(canonical, PresentationMode::Agent, 0),
        OutputFormat::Json,
        PresentationMode::Agent,
    );
    assert_eq!(results.as_array().expect("recall array").len(), 1);
    assert_eq!(results[0]["full_id"], remembered["id"]);
    assert_eq!(results[0]["id"].as_str().unwrap().len(), 8);
    let compact_id = results[0]["id"].clone();
    for target_id in [&compact_id, &remembered["id"]] {
        let result = pack
            .dispatch(
                "brain.auto_feedback",
                json!({
                    "query": "Cobalt recall forwarding",
                    "results": results,
                    "target_id": target_id,
                    "signal": "implicit_positive"
                }),
                &registry,
                &token,
            )
            .await
            .expect("either presented recall alias selects the unchanged result");
        assert_eq!(result["emitted"], true);
        assert_eq!(result["target_id"], remembered["id"]);
        let event_id = result["event_id"].as_str().unwrap().parse().unwrap();
        let event = rt
            .events(&token)
            .expect("event store")
            .get_event(event_id)
            .await
            .unwrap()
            .expect("feedback event");
        assert_eq!(event.payload["candidate_ids"], json!([compact_id]));
    }
}
