//! Public receipt APIs with uninstrumented timing and bounded owned-input batches.
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use khive_channel::{
    DeliveryReceipt, ReceiptVerificationError, SenderReceiptClaim, VerifiedRecipientReceipt,
};
use serde_json::json;
use uuid::Uuid;

#[path = "support/signed_fixture.rs"]
mod signed_fixture;
use signed_fixture::{signed_receipt, SignatureFixture};

const RECEIPT_CHUNK: usize = 128;
const RECEIPT_REPORT_CALLS: usize = 100_000;
const CLAIM_CALLS: usize = 10_000;
type ClaimInput = (Uuid, Uuid, u64);

fn report(scenario: &str, calls: usize, elapsed: Duration) {
    println!(
        "{}",
        json!({"scenario":scenario,"calls":calls,"elapsed_ns":elapsed.as_nanos(),"ns_per_call":elapsed.as_nanos() as f64 / calls as f64})
    );
}

fn signature_batch(fixture: &SignatureFixture, calls: usize) -> Duration {
    let mut all_valid = true;
    let start = Instant::now();
    for _ in 0..calls {
        all_valid &= fixture
            .key
            .verify(black_box(&fixture.input), black_box(&fixture.signature))
            .is_ok();
    }
    black_box(all_valid);
    let elapsed = start.elapsed();
    assert!(all_valid, "all prepared signatures must verify");
    elapsed
}

fn receipt_batch(
    receipt: &DeliveryReceipt,
    pinned: &[u8; 32],
    calls: usize,
    valid: bool,
) -> Duration {
    let mut elapsed = Duration::ZERO;
    let mut verified = 0;
    let mut refused = 0;
    for offset in (0..calls).step_by(RECEIPT_CHUNK) {
        let chunk = RECEIPT_CHUNK.min(calls - offset);
        // Only one bounded chunk exists. Input cloning and result-buffer allocation
        // are outside the timer; draining retains the input buffer until afterwards.
        let mut prepared = vec![receipt.clone(); chunk];
        let mut results = Vec::with_capacity(chunk);
        let start = Instant::now();
        for input in prepared.drain(..) {
            results.push(black_box(VerifiedRecipientReceipt::verify(
                black_box(input),
                black_box(pinned),
            )));
        }
        elapsed += start.elapsed();
        for result in results {
            match result {
                Ok(observed) => {
                    assert!(valid, "bad signature unexpectedly verified");
                    assert_eq!(observed.receipt(), receipt);
                    verified += 1;
                }
                Err(error) => {
                    assert!(!valid, "valid receipt refused: {error:?}");
                    assert_eq!(error, ReceiptVerificationError::InvalidSignature);
                    refused += 1;
                }
            }
        }
    }
    assert_eq!(
        (verified, refused),
        if valid { (calls, 0) } else { (0, calls) }
    );
    elapsed
}

fn claim_batch(inputs: &[ClaimInput]) -> Duration {
    let mut observed_fields = Vec::with_capacity(inputs.len());
    let start = Instant::now();
    for &(message, device, epoch) in black_box(inputs) {
        let claim = black_box(SenderReceiptClaim {
            logical_message_id: Some(message),
            recipient_device_id: Some(device),
            recipient_key_epoch: Some(epoch),
        });
        let observed = black_box((
            claim.logical_message_id.expect("message field"),
            claim.recipient_device_id.expect("device field"),
            claim.recipient_key_epoch.expect("epoch field"),
        ));
        observed_fields.push(observed);
    }
    let elapsed = start.elapsed();
    assert_eq!(
        observed_fields.as_slice(),
        inputs,
        "every claim field must survive build/read"
    );
    elapsed
}

fn lifecycle(c: &mut Criterion) {
    let signature = SignatureFixture::new();
    let mut group = c.benchmark_group("channel_sig_verify");
    group.sample_size(10);
    for calls in [1, 1000] {
        report("sig_verify", calls, signature_batch(&signature, calls));
        group.throughput(Throughput::Elements(calls as u64));
        group.bench_with_input(BenchmarkId::from_parameter(calls), &calls, |b, &calls| {
            b.iter_custom(|iterations| {
                (0..iterations)
                    .map(|_| signature_batch(&signature, calls))
                    .sum::<Duration>()
            })
        });
    }
    group.finish();

    let (valid, pinned) = signed_receipt();
    let mut bad_signature = valid.clone();
    bad_signature.signature[0] ^= 1;
    let mut group = c.benchmark_group("channel_receipt_verify");
    group.sample_size(10);
    group.throughput(Throughput::Elements(RECEIPT_CHUNK as u64));
    for (name, receipt, is_valid) in [("valid", &valid, true), ("bad_sig", &bad_signature, false)] {
        // The fixed report covers the issue's 100k input count once per run;
        // Criterion trends smaller chunks without accumulating 100k owned receipts.
        report(
            name,
            RECEIPT_REPORT_CALLS,
            receipt_batch(receipt, &pinned, RECEIPT_REPORT_CALLS, is_valid),
        );
        group.bench_function(format!("{name}_{RECEIPT_CHUNK}"), |b| {
            b.iter_custom(|iterations| {
                (0..iterations)
                    .map(|_| receipt_batch(receipt, &pinned, RECEIPT_CHUNK, is_valid))
                    .sum::<Duration>()
            })
        });
    }
    group.finish();

    let claims: Vec<_> = (0..CLAIM_CALLS)
        .map(|i| {
            (
                Uuid::from_u128(10_000 + i as u128),
                Uuid::from_u128(50_000 + i as u128),
                1 + i as u64 % 17,
            )
        })
        .collect();
    report(
        "sender_receipt_build_read",
        claims.len(),
        claim_batch(&claims),
    );
    let mut group = c.benchmark_group("channel_sender_receipt");
    group.sample_size(10);
    group.throughput(Throughput::Elements(CLAIM_CALLS as u64));
    group.bench_function("build_read_10000", |b| {
        b.iter_custom(|iterations| {
            (0..iterations)
                .map(|_| claim_batch(&claims))
                .sum::<Duration>()
        })
    });
    group.finish();
}

criterion_group!(benches, lifecycle);
criterion_main!(benches);
