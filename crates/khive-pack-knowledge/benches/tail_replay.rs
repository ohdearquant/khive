//! Knowledge ANN write-to-visible measurements. No timing threshold is a gate.
//! Run in an exclusive host window with an isolated target directory; raw JSON
//! belongs to run artifacts, not the source tree. Each cell starts from its own
//! file-backed warm corpus. OS cache and internal replay duration are unmeasured.

#[path = "support/tail_replay.rs"]
mod support;

fn main() {
    let quick = std::env::args().any(|arg| arg == "--quick");
    let mut sizes = if quick {
        vec![64]
    } else {
        vec![10_000, 100_000]
    };
    if !quick && std::env::var("KHIVE_BENCH_TAIL_500K").as_deref() == Ok("1") {
        sizes.push(500_000);
    }
    let m: usize = std::env::var("KHIVE_BENCH_TAIL_M")
        .ok()
        .map(|value| value.parse().expect("positive M"))
        .unwrap_or(if quick { 4 } else { 16 });
    let hz: f64 = std::env::var("KHIVE_BENCH_TAIL_WRITE_HZ")
        .ok()
        .map(|value| value.parse().expect("positive offered rate"))
        .unwrap_or(10.0);
    let runtime = tokio::runtime::Runtime::new().expect("measurement runtime");
    for n in sizes {
        for arm in [
            support::Arm::OneIndex,
            support::Arm::VerbDelete,
            support::Arm::SameSubject,
            support::Arm::DistinctSubjects,
            support::Arm::ComposedVectorDelete,
        ] {
            let sample = runtime.block_on(support::measure(n, arm, m, hz, false));
            eprintln!(
                "tail-replay {} N={n}: raw={} distinct={} replay_reads={} final_scans={}",
                arm.label(),
                sample.raw_rows,
                sample.distinct_subjects,
                sample.replay_point_reads,
                sample.final_state_scans,
            );
            // All fields are retained for the machine-readable row; no pooling
            // across corpus sizes, cache states or write distributions.
            println!("KNOWLEDGE_TAIL_REPLAY {}", sample.report);
        }
    }
}
