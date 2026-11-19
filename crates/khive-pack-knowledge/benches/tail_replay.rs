//! Knowledge ANN write-to-visible measurements on deterministic synthetic corpora.
//! Default: 10k atoms; 100k/500k are explicit opt-ins. See docs/custom-benchmarks.md.

#[path = "support/tail_replay.rs"]
mod support;
use serde_json::json;

const ARMS: [support::Arm; 5] = [
    support::Arm::OneIndex,
    support::Arm::VerbDelete,
    support::Arm::SameSubject,
    support::Arm::DistinctSubjects,
    support::Arm::ComposedVectorDelete,
];

struct Cases {
    sizes: Vec<usize>,
    m: usize,
    hz: f64,
}
impl Cases {
    fn from_args(args: &[String]) -> Self {
        // `cargo bench` passes `--bench`; `cargo test --benches` runs this binary without it. Like Criterion's test
        // mode, that run is a smoke check, so it takes the quick cohort. Listing and protocol controls keep the default.
        let test_mode = !args.iter().any(|arg| {
            arg == "--bench" || arg == "--list" || arg.starts_with("--regression-case=")
        });
        let quick = test_mode || args.iter().any(|arg| arg == "--quick");
        let mut sizes = if quick { vec![64] } else { vec![10_000] };
        if !quick && std::env::var("KHIVE_BENCH_TAIL_100K").as_deref() == Ok("1") {
            sizes.push(100_000);
        }
        if !quick && std::env::var("KHIVE_BENCH_TAIL_500K").as_deref() == Ok("1") {
            sizes.push(500_000);
        }
        let m = std::env::var("KHIVE_BENCH_TAIL_M")
            .ok()
            .map(|v| v.parse().expect("positive M"))
            .unwrap_or(if quick { 4 } else { 16 });
        let hz: f64 = std::env::var("KHIVE_BENCH_TAIL_WRITE_HZ")
            .ok()
            .map(|v| v.parse().expect("positive offered rate"))
            .unwrap_or(10.0);
        assert!(m > 0 && sizes.iter().all(|n| m < *n));
        assert!(hz.is_finite() && hz > 0.0);
        Self { sizes, m, hz }
    }
    fn id(&self, n: usize, arm: support::Arm) -> String {
        format!(
            "knowledge_tail_replay/synthetic/n_{n}/m_{}/hz_{}/{}/first_search",
            self.m,
            self.hz,
            arm.label()
        )
    }
}

fn emit_result(id: &str, first_search_ns: u64) {
    assert!(
        first_search_ns > 0,
        "zero-duration sample is not a usable reference"
    );
    // One fresh corpus/one observation per case: the median of one is that
    // observation, not a distribution estimate. The full diagnostic row follows.
    println!(
        "BENCH-JSON {}",
        json!({"id":id,"statistic":"median_ns","value":first_search_ns,"n":1})
    );
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let cases = Cases::from_args(&args);
    if args.iter().any(|arg| arg == "--list") {
        for &n in &cases.sizes {
            for arm in ARMS {
                println!("{}: benchmark", cases.id(n, arm));
            }
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
        // Explicit protocol test data, never a benchmark measurement.
        eprintln!("report-contract control: synthetic protocol values, not timings");
        for &n in &cases.sizes {
            for arm in ARMS {
                emit_result(&cases.id(n, arm), 123);
            }
        }
        return;
    }
    let runtime = tokio::runtime::Runtime::new().expect("measurement runtime");
    for &n in &cases.sizes {
        for arm in ARMS {
            let sample = runtime.block_on(support::measure(n, arm, cases.m, cases.hz, false));
            emit_result(
                &cases.id(n, arm),
                sample.report["first_search_ns"]
                    .as_u64()
                    .expect("first search nanoseconds fit u64"),
            );
            eprintln!(
                "tail-replay {} N={n}: raw={} distinct={} replay_reads={} final_scans={}",
                arm.label(),
                sample.raw_rows,
                sample.distinct_subjects,
                sample.replay_point_reads,
                sample.final_state_scans
            );
            // Preserve all visibility, custody, route and replay diagnostics.
            println!("KNOWLEDGE_TAIL_REPLAY {}", sample.report);
        }
    }
}
