use std::hint::black_box;
use std::time::{Duration, Instant};

const SIZES: [usize; 4] = [64, 4 * 1024, 1024 * 1024, 64 * 1024 * 1024];
const SAMPLE_TARGET: Duration = Duration::from_millis(250);
const SAMPLES: usize = 7;

fn hash_for(data: &[u8], iterations: usize) -> (Duration, u8) {
    let start = Instant::now();
    let mut checksum = 0;
    for iteration in 0..iterations {
        let digest = blake3::hash(black_box(data));
        checksum ^= digest.as_bytes()[iteration % 32];
        black_box(checksum);
    }
    (start.elapsed(), checksum)
}

fn calibrated_iterations(data: &[u8]) -> usize {
    let mut iterations = 1;
    loop {
        let (elapsed, _) = hash_for(data, iterations);
        if elapsed >= SAMPLE_TARGET {
            return iterations;
        }
        let scale = (SAMPLE_TARGET.as_nanos() / elapsed.as_nanos().max(1)) as usize;
        iterations = iterations.saturating_mul(scale.clamp(2, 64));
    }
}

fn main() {
    let backend = if cfg!(feature = "pure") {
        "pure"
    } else {
        "default"
    };
    println!("backend={backend} samples={SAMPLES}");

    for size in SIZES {
        let data: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
        let iterations = calibrated_iterations(&data);
        let mut samples = Vec::with_capacity(SAMPLES);
        let mut checksum = 0;
        for _ in 0..SAMPLES {
            let (elapsed, sample_checksum) = hash_for(&data, iterations);
            samples.push(elapsed.as_secs_f64());
            checksum ^= sample_checksum;
        }
        samples.sort_by(f64::total_cmp);
        let median_seconds = samples[SAMPLES / 2];
        let mib = size as f64 * iterations as f64 / (1024.0 * 1024.0);
        println!(
            "bytes={size} iterations={iterations} median_mib_s={:.2} min_s={:.6} max_s={:.6} checksum={checksum}",
            mib / median_seconds,
            samples[0],
            samples[SAMPLES - 1],
        );
    }
}
