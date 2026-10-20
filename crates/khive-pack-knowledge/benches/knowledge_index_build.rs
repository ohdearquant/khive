#[path = "knowledge_support/fixture.rs"]
mod fixture;

use fixture::{check_search, executor, Fixture, QUERY};
use serde_json::json;

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    // Cargo passes --bench to harness=false targets only for cargo bench.
    // Criterion-style flags are accepted without multiplying full builds.
    let smoke = !args.iter().any(|arg| arg == "--bench") || args.iter().any(|arg| arg == "--test");
    let mut sizes = if smoke {
        vec![1000]
    } else {
        vec![1000, 10_000]
    };
    if !smoke && std::env::var("KHIVE_BENCH_KNOWLEDGE_100K").as_deref() == Ok("1") {
        sizes.push(100_000);
    }
    let rt = executor();
    for count in sizes {
        let fixture = Fixture::new(&rt, true);
        rt.block_on(fixture.seed_atoms(count, false));
        let elapsed = fixture.index(&rt, count);
        let params = json!({"query":QUERY, "kind":"atom", "limit":20, "rerank":true});
        let (_, result) = fixture.measure(&rt, "knowledge.search", params);
        check_search(&result, count, false, false);
        println!(
            "{}",
            json!({"scenario":"knowledge_index_build", "atoms":count,
            "actual_builds":1, "elapsed_ms":elapsed.as_secs_f64()*1000.0,
            "vectors_per_second":count as f64 / elapsed.as_secs_f64(),
            "provider":"synthetic normalized 384-D", "storage":"fresh private queued WAL"})
        );
        rt.block_on(fixture.close());
    }
    if smoke {
        println!("Success");
    }
}
