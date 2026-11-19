#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
#[path = "support/fixtures.rs"]
mod fixtures;
#[cfg(not(target_arch = "wasm32"))]
#[path = "support/selection.rs"]
mod selection;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    use fixtures::{ParseFixture, Stream};
    use std::hint::black_box;
    use std::time::Instant;

    let selection = selection::Selection::from_args();
    for stream in Stream::ALL {
        for count in [1_000, 50_000, 500_000] {
            let name = format!("parse/{}/{count}", stream.name());
            if !selection.selected(&name, count) {
                continue;
            }
            let fixture = ParseFixture::new(stream, count);
            let start = Instant::now();
            let graph = black_box(&fixture).parse();
            let elapsed = start.elapsed();
            fixture.validate(&graph);
            println!(
                "{}",
                serde_json::json!({
                    "scenario": name, "status": "MEASURED", "statistic": "single_call",
                    "calls": 1, "records": count, "input_utf8_bytes": fixture.text.len(),
                    "ns_per_call": elapsed.as_nanos(),
                    "ns_per_record": elapsed.as_nanos() as f64 / count as f64,
                    "boundary": "public from_ndjson including result allocation; fixture, validation and result drop excluded"
                })
            );
            // Both graph and input drop here before any other scene is constructed.
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
