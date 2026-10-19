use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use criterion::Criterion;
use serde_json::json;

use crate::fixture::{check_search, executor, slug, Fixture, CONTENT, QUERY};

const SAMPLES: usize = 21;
const BRIEFING_QUERY: &str =
    "knowledge retrieval benchmark generated domain briefing evidence budget selection";

fn bench(c: &mut Criterion, name: &str, sample: impl Fn() -> (Duration, Option<usize>)) {
    // Warm-up and result validation use the same dispatch as the measured calls.
    sample();
    let mut measured: Vec<_> = (0..SAMPLES).map(|_| sample()).collect();
    measured.sort_unstable_by_key(|sample| sample.0);
    let median = measured[10].0;
    let p95 = measured[19].0;
    let tokens: usize = measured.iter().filter_map(|sample| sample.1).sum();
    let elapsed: Duration = measured.iter().map(|sample| sample.0).sum();
    println!(
        "{}",
        json!({"scenario":name, "actual_call_samples":SAMPLES,
        "p50_us":median.as_secs_f64()*1e6, "p95_us":p95.as_secs_f64()*1e6,
        "percentile_method":"nearest rank of individual calls (11/20 of 21)",
        "estimated_rendered_tokens_per_second":(tokens > 0).then(|| tokens as f64 / elapsed.as_secs_f64()),
        "token_estimator":"ceil rendered UTF-8 bytes / 4; not tokenizer output"})
    );
    c.bench_function(name, |b| {
        b.iter_custom(|iterations| (0..iterations).map(|_| sample().0).sum())
    });
}

pub fn compose_suggest(c: &mut Criterion) {
    let rt = executor();
    for (count, domains) in [(100, 10), (1000, 20)] {
        let fixture = Fixture::new(&rt, false);
        rt.block_on(fixture.seed_atoms(count, false));
        let mut expected = BTreeMap::new();
        let domain_rows: Vec<_> = (0..domains)
            .map(|d| {
                let name = format!("knowledge retrieval benchmark domain {d}");
                let members: Vec<_> = (d..count).step_by(domains).map(slug).collect();
                expected.insert(name.clone(), members.len());
                json!({"slug":format!("generated-domain-{d}"), "name":name,
                "description":CONTENT, "members":members})
            })
            .collect();
        let result = rt
            .block_on(
                fixture
                    .registry
                    .dispatch("knowledge.upsert_domains", json!({"domains":domain_rows})),
            )
            .unwrap();
        assert_eq!(result["created"], domains);
        assert_eq!(result["updated"], 0);
        let pins: Vec<_> = (0..count).map(slug).collect();
        let params = json!({"atom_ids":pins, "query":BRIEFING_QUERY, "max_tokens":2000});
        bench(
            c,
            &format!("knowledge_compose/atoms{count}_domains{domains}"),
            || {
                let prepared = params.clone();
                let (elapsed, result) = fixture.measure(&rt, "knowledge.compose", prepared);
                assert_eq!(result["status"], "ok");
                let data = &result["data"];
                assert!(data.get("degraded").is_none(), "{result}");
                let markdown = data["markdown"].as_str().unwrap();
                assert!(markdown.len() <= 2000 * 4);
                let rows = data["atoms"].as_array().unwrap();
                assert!(!rows.is_empty());
                assert_eq!(data["count"], rows.len());
                let mut actual = BTreeSet::new();
                for row in rows {
                    let atom_slug = row["slug"].as_str().unwrap();
                    assert!(pins.iter().any(|pin| pin == atom_slug));
                    assert!(actual.insert(atom_slug));
                    assert!(markdown.contains(&format!("Source: {atom_slug}\n")));
                }
                (elapsed, Some(markdown.len().div_ceil(4)))
            },
        );
        let params = json!({"query":BRIEFING_QUERY, "limit":domains});
        bench(
            c,
            &format!("knowledge_suggest/atoms{count}_domains{domains}"),
            || {
                let prepared = params.clone();
                let (elapsed, result) = fixture.measure(&rt, "knowledge.suggest", prepared);
                assert!(result.get("degraded").is_none(), "{result}");
                let rows = result["results"].as_array().unwrap();
                assert_eq!(rows.len(), domains);
                assert_eq!(result["total"], domains);
                let mut names = BTreeSet::new();
                for row in rows {
                    let name = row["name"].as_str().unwrap();
                    assert!(names.insert(name));
                    assert_eq!(row["members"], expected[name]);
                    assert!(row["size"].as_u64().unwrap() > 0);
                }
                (elapsed, None)
            },
        );
        rt.block_on(fixture.close());
    }
}

pub fn fold(c: &mut Criterion) {
    let rt = executor();
    let fixture = Fixture::new(&rt, false);
    for count in [100, 1000, 10_000] {
        let candidates: Vec<_> = (0..count)
            .map(|i| {
                json!({"id":format!("candidate-{i}"),
            "score":1.0 - i as f64 / (count + 1) as f64, "size":32, "members":1})
            })
            .collect();
        for budget in [256, 2048] {
            let params = json!({"candidates":candidates, "budget":budget});
            bench(
                c,
                &format!("knowledge_fold/candidates{count}_budget{budget}"),
                || {
                    let prepared = params.clone();
                    let (elapsed, result) = fixture.measure(&rt, "knowledge.fold", prepared);
                    let rows = result["selected"].as_array().unwrap();
                    assert_eq!(rows.len(), budget / 32);
                    assert_eq!(result["selected_count"], rows.len());
                    assert_eq!(result["budget"], budget);
                    let mut ids = BTreeSet::new();
                    let mut sum = 0;
                    for row in rows {
                        let id = row["id"].as_str().unwrap();
                        assert!(ids.insert(id));
                        let i: usize = id.strip_prefix("candidate-").unwrap().parse().unwrap();
                        assert!(i < count);
                        assert_eq!(row["size"], 32);
                        sum += 32;
                    }
                    assert_eq!(result["total_size"], sum);
                    assert_eq!(sum, budget);
                    (elapsed, None)
                },
            );
        }
    }
    rt.block_on(fixture.close());
}

pub fn search_scale(c: &mut Criterion) {
    let rt = executor();
    for count in [100, 1000, 10_000] {
        let fixture = Fixture::new(&rt, true);
        rt.block_on(fixture.seed_atoms(count, false));
        fixture.index(&rt, count);
        let params = json!({"query":QUERY, "kind":"atom", "limit":20, "rerank":true});
        bench(c, &format!("knowledge_search_scale/atoms{count}"), || {
            let prepared = params.clone();
            let (elapsed, result) = fixture.measure(&rt, "knowledge.search", prepared);
            check_search(&result, count, false, false);
            (elapsed, None)
        });
        rt.block_on(fixture.close());
    }
}

pub fn search_matrix(c: &mut Criterion) {
    let rt = executor();
    let fixture = Fixture::new(&rt, true);
    rt.block_on(fixture.seed_atoms(1000, true));
    fixture.index(&rt, 1000);
    for alpha in [0.0, 0.5, 1.0] {
        for drafts in [false, true] {
            let params = json!({"query":QUERY, "kind":"atom", "limit":20,
                "rerank":true, "rerank_alpha":alpha, "include_drafts":drafts});
            bench(
                c,
                &format!("knowledge_search_matrix/alpha{alpha}_drafts{drafts}"),
                || {
                    let prepared = params.clone();
                    let (elapsed, result) = fixture.measure(&rt, "knowledge.search", prepared);
                    check_search(&result, 1000, true, drafts);
                    (elapsed, None)
                },
            );
        }
    }
    rt.block_on(fixture.close());
}
