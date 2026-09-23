mod search_ranking_tests {
    use super::*;
    use crate::coordinator::{BackendSearchResult, CoordError, CoordLinkResult};
    use khive_pack_kg::handlers::{SearchSubstrate, ValidatedSearchRequest};
    use khive_runtime::{
        BackendId, NoteSearchHit, RankScoreKind, SearchHit, SearchSignals, SearchSource,
        StorageBackend,
    };
    use khive_score::DeterministicScore;
    use khive_storage::EdgeRelation;
    use uuid::Uuid;

    struct RankingCoordinator {
        kind: RankScoreKind,
        signals: SearchSignals,
        failed: bool,
    }

    #[async_trait::async_trait]
    impl CoordinatorService for RankingCoordinator {
        async fn locate(&self, _id: Uuid) -> Option<BackendId> {
            None
        }
        fn record_created(&self, _id: Uuid, _backend_id: BackendId) {}
        fn primary_backend_id(&self) -> Option<BackendId> {
            Some(BackendId::main())
        }
        #[allow(clippy::too_many_arguments)]
        async fn link(
            &self,
            _namespace: &Namespace,
            _source_id: Uuid,
            _target_id: Uuid,
            _relation: EdgeRelation,
            _weight: f64,
            _metadata: Option<Value>,
            _resurrect: bool,
        ) -> Result<CoordLinkResult, CoordError> {
            Err(CoordError::Backend(
                "fixture does not implement links".into(),
            ))
        }
        async fn fan_out_search(
            &self,
            request: &ValidatedSearchRequest,
            _namespace: &Namespace,
            _extra_visible: &[Namespace],
        ) -> CoordSearchResult {
            // Deliberately leave filtering to the MCP boundary under test.
            let id = Uuid::from_u128(1);
            let score = DeterministicScore::from_raw(1_i64 << 31);
            let is_note = request.substrate() == SearchSubstrate::Note;
            let source = match (self.signals.vector_similarity, self.signals.keyword_score) {
                (Some(_), Some(_)) => SearchSource::Both,
                (Some(_), None) => SearchSource::Vector,
                _ => SearchSource::Text,
            };
            let entity_hits = if is_note {
                vec![]
            } else {
                vec![SearchHit {
                    entity_id: id,
                    score,
                    rank_score_kind: self.kind,
                    signals: self.signals,
                    source,
                    title: Some("ranking fixture".into()),
                    snippet: None,
                }]
            };
            let note_hits = if is_note {
                vec![NoteSearchHit {
                    note_id: id,
                    score,
                    rank_score_kind: self.kind,
                    signals: self.signals,
                    source,
                    title: Some("ranking fixture".into()),
                    snippet: None,
                }]
            } else {
                vec![]
            };
            let mut per_backend = vec![BackendSearchResult {
                backend_id: BackendId::main(),
                entity_hits: entity_hits.clone(),
                note_hits: note_hits.clone(),
                vector_selected: self.signals.vector_similarity.is_some(),
                error: None,
                vector_error: None,
            }];
            if self.failed {
                per_backend.push(BackendSearchResult {
                    backend_id: BackendId::parse("archive").unwrap(),
                    entity_hits: vec![],
                    note_hits: vec![],
                    vector_selected: false,
                    error: Some(BackendSearchFailure::backend("storage unavailable")),
                    vector_error: None,
                });
            }
            CoordSearchResult {
                entity_hits,
                note_hits,
                per_backend,
                partial: self.failed,
                entity_kinds: [(id, "concept".into())].into_iter().collect(),
                note_kinds: [(id, "observation".into())].into_iter().collect(),
                entity_created_at: [(id, 1_000_000)].into_iter().collect(),
                entity_updated_at: [(id, 2_000_000)].into_iter().collect(),
                note_created_at: [(id, 1_000_000)].into_iter().collect(),
                note_updated_at: [(id, 2_000_000)].into_iter().collect(),
                note_versions: [(id, 1)].into_iter().collect(),
                note_names: [(id, Some("stored name".into()))].into_iter().collect(),
            }
        }
        fn is_single_backend(&self) -> bool {
            false
        }
    }

    fn server(coordinator: Option<RankingCoordinator>) -> KhiveMcpServer {
        let backend = Arc::new(StorageBackend::memory().expect("memory backend"));
        backend.prepare_core_schema().expect("core schema");
        let runtime = KhiveRuntime::from_backend(
            backend,
            RuntimeConfig {
                db_path: None,
                events_split: None,
                actor_id: Some("test:search-ranking-wire".into()),
                packs: vec!["kg".into()],
                ..RuntimeConfig::no_embeddings()
            },
        );
        let server = KhiveMcpServer::new(runtime).expect("KG MCP server");
        match coordinator {
            Some(coordinator) => server.with_coordinator(Arc::new(coordinator)),
            None => server,
        }
    }

    async fn search(server: &KhiveMcpServer, args: Value, presentation: &str) -> Value {
        let response = server
            .dispatch_request_local(RequestParams {
                ops: json!([{"tool": "search", "args": args}]).to_string(),
                presentation: Some(presentation.into()),
                format: Some("json".into()),
                ..Default::default()
            })
            .await
            .expect("MCP request");
        let envelope: Value = serde_json::from_str(&response).expect("JSON response");
        assert_eq!(envelope["results"].as_array().unwrap().len(), 1);
        envelope["results"][0].clone()
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn coordinator_ranking_fields_cover_both_substrates_and_all_kinds() {
        for kind in [
            RankScoreKind::Rrf,
            RankScoreKind::Vector,
            RankScoreKind::Keyword,
            RankScoreKind::Weighted,
            RankScoreKind::Union,
        ] {
            for signals in [
                SearchSignals::default(),
                SearchSignals {
                    vector_similarity: Some(DeterministicScore::ZERO),
                    keyword_score: None,
                },
                SearchSignals {
                    vector_similarity: None,
                    keyword_score: Some(DeterministicScore::from_f64(18.375)),
                },
                SearchSignals {
                    vector_similarity: Some(DeterministicScore::from_f64(0.75)),
                    keyword_score: Some(DeterministicScore::from_f64(18.375)),
                },
            ] {
                let server = server(Some(RankingCoordinator {
                    kind,
                    signals,
                    failed: false,
                }));
                for substrate in ["entity", "note"] {
                    for presentation in ["verbose", "agent"] {
                        let entry = search(
                            &server,
                            json!({"kind": substrate, "query": "ranking"}),
                            presentation,
                        )
                        .await;
                        assert_eq!(entry["ok"], true);
                        assert_eq!(entry["status"], "complete");
                        let hits = entry["result"].as_array().expect("hit array");
                        assert_eq!(hits.len(), 1);
                        let hit = &hits[0];
                        assert_eq!(hit["rank_score"], json!(0.5));
                        assert_eq!(hit["score"], hit["rank_score"]);
                        assert_eq!(hit["rank_score_kind"], kind.as_str());
                        if signals == SearchSignals::default() {
                            if presentation == "verbose" {
                                assert_eq!(hit["signals"], json!({}));
                            } else {
                                assert!(hit.get("signals").is_none());
                            }
                        } else {
                            let evidence = hit["signals"].as_object().expect("retained signals");
                            assert_eq!(
                                evidence.contains_key("vector_similarity"),
                                signals.vector_similarity.is_some()
                            );
                            assert_eq!(
                                evidence.contains_key("keyword_score"),
                                signals.keyword_score.is_some()
                            );
                            if let Some(vector) = signals.vector_similarity {
                                assert_eq!(evidence["vector_similarity"], json!(vector.to_f64()));
                            }
                            if signals.keyword_score.is_some() {
                                assert_eq!(
                                    evidence["keyword_score"],
                                    json!(if presentation == "verbose" {
                                        18.375
                                    } else {
                                        18.4
                                    })
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn coordinator_floor_quantizes_input_and_preserves_inclusive_equality() {
        let server = server(Some(RankingCoordinator {
            kind: RankScoreKind::Rrf,
            signals: SearchSignals::default(),
            failed: false,
        }));
        for substrate in ["entity", "note"] {
            for spelling in ["min_rank_score", "min_score"] {
                for floor in [0.5, 0.5 + 0.25 / 4_294_967_296.0] {
                    let mut args = json!({"kind": substrate, "query": "ranking", "limit": 1});
                    args[spelling] = json!(floor);
                    let entry = search(&server, args, "verbose").await;
                    assert_eq!(entry["ok"], true);
                    assert_eq!(entry["result"].as_array().unwrap().len(), 1);
                    assert_eq!(entry["result"][0]["rank_score"], json!(0.5));
                }
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn coordinator_rank_floor_preserves_degraded_empty_error() {
        let server = server(Some(RankingCoordinator {
            kind: RankScoreKind::Rrf,
            signals: SearchSignals::default(),
            failed: true,
        }));
        for substrate in ["entity", "note"] {
            let partial = search(
                &server,
                json!({"kind": substrate, "query": "ranking", "min_rank_score": 0.5}),
                "verbose",
            )
            .await;
            assert_eq!(partial["ok"], true);
            assert_eq!(partial["status"], "partial");
            assert_eq!(partial["partial"], true);
            assert_eq!(partial["result"].as_array().unwrap().len(), 1);
            for spelling in ["min_rank_score", "min_score"] {
                let mut args = json!({"kind": substrate, "query": "ranking"});
                args[spelling] = json!(0.75);
                for mode in ["verbose", "agent"] {
                    let entry = search(&server, args.clone(), mode).await;
                    assert_eq!(entry["ok"], false);
                    assert!(entry.get("result").is_none());
                    assert_eq!(entry["error"]["kind"], "search_incomplete");
                    assert_eq!(entry["error"]["missing_backends"], json!(["archive"]));
                    assert_eq!(entry["error"]["retryable"], false);
                    assert_eq!(
                        entry["error"]["arm_participation"]["text"]["candidate_count"],
                        0
                    );
                }
            }
        }
    }
}
