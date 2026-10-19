#[path = "knowledge_support/fixture.rs"]
mod fixture;

use fixture::{check_search, executor, Fixture, QUERY};
use serde_json::json;
use std::time::Duration;

fn measure_build(rt: &tokio::runtime::Runtime, count: usize) -> Duration {
    let fixture = Fixture::new(rt, true);
    rt.block_on(fixture.seed_atoms(count, false));
    // Preserve the original boundary: one full public index dispatch, including
    // synthetic embedding, vector writes, ANN construction and publication.
    let elapsed = fixture.index(rt, count);
    let params = json!({"query":QUERY, "kind":"atom", "limit":20, "rerank":true});
    let (_, result) = fixture.measure(rt, "knowledge.search", params);
    check_search(&result, count, false, false);
    rt.block_on(fixture.close());
    elapsed
}

fn case_id(count: usize) -> String {
    format!("knowledge_index_build/atoms{count}")
}

fn emit_result(count: usize, elapsed_ns: u64) {
    assert!(elapsed_ns > 0, "zero-duration build is not a reference");
    // One fresh build produces one observation. Its median is that observation,
    // not a latency distribution or a percentile of repeated requests.
    println!(
        "BENCH-JSON {}",
        json!({"id":case_id(count), "statistic":"median_ns", "value":elapsed_ns, "n":1})
    );
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let list = args.iter().any(|arg| arg == "--list");
    // List the reference cohort even when invoked directly without --bench.
    // The existing test mode keeps the small corpus and runs one build.
    let smoke = (!list && !args.iter().any(|arg| arg == "--bench"))
        || args.iter().any(|arg| arg == "--test");
    let mut sizes = if smoke {
        vec![1000]
    } else {
        vec![1000, 10_000]
    };
    if !smoke && std::env::var("KHIVE_BENCH_KNOWLEDGE_100K").as_deref() == Ok("1") {
        sizes.push(100_000);
    }
    if list {
        for count in sizes {
            println!("{}: benchmark", case_id(count));
        }
        return;
    }
    assert!(
        !args.iter().any(|arg| arg == "--regression-case=list-only"),
        "listing entered workload setup"
    );

    if args
        .iter()
        .any(|arg| arg == "--regression-case=report-contract")
    {
        eprintln!("report-contract control: synthetic protocol values, not timings");
        for count in sizes {
            emit_result(count, 123);
        }
        return;
    }

    let rt = executor();
    for count in sizes {
        let elapsed = measure_build(&rt, count);
        emit_result(
            count,
            elapsed
                .as_nanos()
                .try_into()
                .expect("index duration fits u64 nanoseconds"),
        );
        eprintln!(
            "KNOWLEDGE_INDEX_BUILD {}",
            json!({"scenario":"knowledge_index_build", "atoms":count,
                "actual_builds":1, "elapsed_ms":elapsed.as_secs_f64()*1000.0,
                "vectors_per_second":count as f64 / elapsed.as_secs_f64(),
                "provider":"synthetic normalized 384-D", "storage":"fresh private queued WAL"})
        );
    }
    if smoke {
        println!("Success");
    }
}
