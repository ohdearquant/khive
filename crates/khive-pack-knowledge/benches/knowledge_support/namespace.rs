#[path = "../../src/knowledge/search/lexical_sql.rs"]
mod lexical_sql;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::{json, Value};

use crate::fixture::{executor, Fixture, CONTENT};

// Exactly 17 whitespace-delimited words; no explicit pins bypass auto-suggest.
const QUERY: &str = "knowledge retrieval benchmark generated atom corpus vector embedding search ranking domain briefing evidence budget selection normalized synthetic";
const FOREIGN_ROWS: usize = 20_000;
const STORE_ATOMS: usize = 180_474;
const STORE_DOMAINS: usize = 25_504;

async fn seed_atoms(fixture: &Fixture, namespace: &str, count: usize) {
    if count == 0 {
        return;
    }
    for start in (0..count).step_by(2000) {
        let end = (start + 2000).min(count);
        let mut writer = fixture.runtime.sql().writer().await.unwrap();
        let inserted = writer
            .execute(SqlStatement::new(
                "WITH RECURSIVE n(i) AS (SELECT ?5 UNION ALL SELECT i+1 FROM n WHERE i+1 < ?1) \
         INSERT INTO knowledge_atoms \
         (id,namespace,slug,name,content,tags,finalized,status,created_at,updated_at) \
         SELECT printf('35140000-0000-4000-%s-%012d',?2,i),?3, \
                printf('namespace-atom-%06d',i),printf('knowledge retrieval atom %06d',i), \
                ?4,'[]',1,'reviewed',1,1 FROM n",
                vec![
                    SqlValue::Integer(end as i64),
                    SqlValue::Text(if namespace == "local" { "9000" } else { "8000" }.into()),
                    SqlValue::Text(namespace.into()),
                    SqlValue::Text(CONTENT.into()),
                    SqlValue::Integer(start as i64),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(inserted, (end - start) as u64);
    }
}

async fn seed_domains(fixture: &Fixture, count: usize, atoms: usize) {
    // Deterministic canonical rows and matching mirrors, with one live member
    // per domain. Raw fixture setup is outside every measurement interval.
    let mut writer = fixture.runtime.sql().writer().await.unwrap();
    let rows = writer.execute_batch(vec![
        SqlStatement::new(
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i+1 FROM n WHERE i+1 < ?1) \
             INSERT INTO knowledge_domains \
             (id,namespace,slug,name,description,tags,members,created_at,updated_at) \
             SELECT printf('35140000-0000-4000-a000-%012d',i),'local', \
                    printf('namespace-domain-%06d',i),printf('knowledge retrieval domain %06d',i), \
                    ?2,'[\"type:domain\"]',json_array(printf('namespace-atom-%06d',i % ?3)),1,1 FROM n",
            vec![SqlValue::Integer(count as i64), SqlValue::Text(CONTENT.into()),
                 SqlValue::Integer(atoms as i64)],
        ),
        SqlStatement::new(
            "INSERT INTO knowledge_atoms \
             (id,namespace,slug,name,content,tags,properties,finalized,status,created_at,updated_at) \
             SELECT id,namespace,slug,name,description,tags,json_object('members',json(members)), \
                    1,'reviewed',created_at,updated_at FROM knowledge_domains",
            vec![],
        ),
    ]).await.unwrap();
    assert_eq!(rows, 2 * count as u64);
}

fn check_suggest(result: &Value, domains: usize) {
    assert!(result.get("degraded").is_none(), "{result}");
    let rows = result["results"].as_array().unwrap();
    assert_eq!(rows.len(), domains);
    assert_eq!(result["total"], domains);
    let actual: BTreeSet<_> = rows
        .iter()
        .map(|row| {
            assert_eq!(row["members"], 1);
            assert!(row["size"].as_u64().unwrap() > 0);
            row["name"].as_str().unwrap().to_owned()
        })
        .collect();
    let expected = (0..domains)
        .map(|i| format!("knowledge retrieval domain {i:06}"))
        .collect();
    assert_eq!(actual, expected);
}

fn report(name: &str, foreign: usize, samples: &mut [Duration]) {
    samples.sort_unstable();
    let rank = |percent: usize| (samples.len() * percent).div_ceil(100) - 1;
    println!(
        "{}",
        json!({
            "scenario": name, "foreign_matching_atoms": foreign, "samples": samples.len(),
            "p50_us": samples[rank(50)].as_secs_f64() * 1e6,
            "p95_us": samples[rank(95)].as_secs_f64() * 1e6,
            "percentiles": "nearest rank of individual calls", "storage": "private queued WAL",
            "provider": "none"
        })
    );
}

pub fn run(samples: usize, full: bool) {
    assert_eq!(QUERY.split_whitespace().count(), 17);
    let rt = executor();
    for foreign in [0, FOREIGN_ROWS] {
        let fixture = Fixture::new(&rt, false);
        rt.block_on(async {
            seed_atoms(&fixture, "foreign", foreign).await;
            seed_atoms(&fixture, "local", 10).await;
            seed_domains(&fixture, 3, 10).await;
        });
        let sql = lexical_sql::eligibility_fallback_sql(
            " AND (status IS NULL OR (status != ?4 AND status != ?5))",
            " AND khive_tag_contains(a.tags, 'type:domain')",
        );
        let statement = SqlStatement::new(
            sql,
            vec![
                SqlValue::Text("\"knowledge\"".into()),
                SqlValue::Text("local".into()),
                SqlValue::Integer(3),
                SqlValue::Text("draft".into()),
                SqlValue::Text("deprecated".into()),
            ],
        );
        let mut fallback = Vec::new();
        let mut suggest = Vec::new();
        for sample in 0..=samples {
            let prepared = statement.clone();
            let (elapsed, rows) = rt.block_on(async {
                let mut reader = fixture.runtime.sql().reader().await.unwrap();
                let start = Instant::now();
                let rows = reader.query_all(prepared).await;
                (start.elapsed(), rows.unwrap())
            });
            assert_eq!(rows.len(), 3);
            for (i, row) in rows.iter().enumerate() {
                assert_eq!(
                    row.text("slug").unwrap(),
                    format!("namespace-domain-{i:06}")
                );
            }
            let (dispatch, result) = fixture.measure(
                &rt,
                "knowledge.suggest",
                json!({"query": QUERY, "limit": 3}),
            );
            check_suggest(&result, 3);
            if sample > 0 {
                fallback.push(elapsed);
                suggest.push(dispatch);
            }
        }
        report(
            "knowledge_namespace/eligibility_fallback_sql",
            foreign,
            &mut fallback,
        );
        report("knowledge_namespace/full_suggest", foreign, &mut suggest);
        rt.block_on(fixture.close());
    }
    if full {
        let fixture = Fixture::new(&rt, false);
        rt.block_on(async {
            seed_atoms(&fixture, "local", STORE_ATOMS).await;
            seed_domains(&fixture, STORE_DOMAINS, STORE_ATOMS).await;
        });
        let (_, stats) = fixture.measure(&rt, "knowledge.stats", json!({}));
        assert_eq!(stats["total_atoms"], STORE_ATOMS);
        assert_eq!(stats["total_domains"], STORE_DOMAINS);
        // Three bounded individual calls, including the first call after setup.
        // A missed target is evidence, not an invitation to retry until success.
        for sample in 0..3 {
            let (elapsed, result) = fixture.measure(
                &rt,
                "knowledge.compose",
                json!({"query": QUERY, "max_tokens": 1500}),
            );
            assert_eq!(result["status"], "ok");
            let data = &result["data"];
            assert!(data.get("degraded").is_none(), "{result}");
            assert!(data.get("suggest_error").is_none(), "{result}");
            assert!(data["count"].as_u64().unwrap() > 0, "{result}");
            let markdown = data["markdown"].as_str().unwrap();
            assert!(markdown.len() <= 1500 * 4);
            println!(
                "{}",
                json!({
                    "scenario": "knowledge_namespace/full_compose_store_size",
                    "sample": sample, "local_atoms": STORE_ATOMS, "local_domains": STORE_DOMAINS,
                    "query_words": 17, "max_tokens": 1500,
                    "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
                    "absolute_target_ms": 6000, "under_target": elapsed < Duration::from_secs(6),
                    "returned_atoms": data["count"], "rendered_bytes": markdown.len(),
                    "provider": "none", "corpus": "synthetic identical matching content; one member per domain"
                })
            );
        }
        rt.block_on(fixture.close());
    }
}
