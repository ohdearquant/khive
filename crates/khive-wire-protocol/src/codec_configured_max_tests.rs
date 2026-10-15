use super::*;
use crate::frame::Frame;

fn oversized_frame() -> Frame {
    Frame::Request {
        id: crate::frame::OperationId("x".into()),
        ops: "a".repeat(4096),
        deadline_ms: None,
        namespace: None,
        actor_id: None,
        visible_namespaces: None,
    }
}

#[test]
fn codec_encode_honors_its_own_maximum_not_the_default() {
    let codec = FrameCodec::new(1024);
    let err = codec.encode(&oversized_frame()).unwrap_err();
    match err {
        CodecError::FrameTooLarge { max, .. } => assert_eq!(max, 1024),
        other => panic!("expected FrameTooLarge with the configured max, got {other:?}"),
    }
}

#[test]
fn codec_encode_accepts_a_frame_within_its_own_maximum() {
    let codec = FrameCodec::new(1024 * 1024);
    let wire = codec
        .encode(&oversized_frame())
        .expect("within configured max");
    assert_eq!(codec.decode(&wire).unwrap(), oversized_frame());
}

#[test]
fn free_function_encode_still_uses_the_default_maximum() {
    let wire = encode_frame(&oversized_frame()).expect("well under 8 MiB");
    assert!(wire.len() > 4096);
}

#[test]
fn codec_configured_above_u32_capacity_still_encodes_normal_frames() {
    // What the old `encode_never_exceeds_the_u32_prefix_capacity` test
    // actually verified (and all it could verify without a >4 GiB
    // payload): configuring a codec above the u32 prefix capacity does
    // not disturb ordinary encoding. It did NOT exercise the u32 guard
    // itself — that decision logic is tested directly below.
    assert!(encode_frame_with_max(&oversized_frame(), usize::MAX).is_ok());
}

#[test]
fn encode_size_guard_reports_the_configured_max_when_it_is_binding() {
    let err = check_encode_payload_len(32, 16).unwrap_err();
    assert_eq!(
        err,
        CodecError::FrameTooLarge {
            declared: 32,
            max: 16
        }
    );
}

#[cfg(target_pointer_width = "64")]
#[test]
fn encode_size_guard_reports_the_u32_prefix_capacity_when_it_is_binding() {
    // The u32 guard fires only when the configured maximum admitted the
    // payload (payload_len <= max) but the 4-byte prefix cannot name
    // it (payload_len > u32::MAX). The configured maximum was NOT
    // exceeded at that point, so the error must name the prefix
    // capacity as the binding limit — reporting the configured max
    // would claim an exceedance that did not happen.
    let err = check_encode_payload_len(u32::MAX as usize + 1, usize::MAX).unwrap_err();
    assert_eq!(
        err,
        CodecError::U32PrefixLimitExceeded {
            declared: u32::MAX as usize + 1,
            max: u32::MAX as usize
        }
    );
    // Exactly at the prefix capacity is still encodable in principle.
    assert!(check_encode_payload_len(u32::MAX as usize, usize::MAX).is_ok());
}
