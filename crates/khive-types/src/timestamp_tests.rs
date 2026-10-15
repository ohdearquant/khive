use super::*;

#[test]
fn conversions() {
    let ts = Timestamp::from_secs(1700000000);
    assert_eq!(ts.as_secs(), 1700000000);
    assert_eq!(ts.as_millis(), 1700000000000);
    assert_eq!(ts.as_micros(), 1700000000000000);
}

#[test]
fn ordering() {
    let a = Timestamp::from_secs(1);
    let b = Timestamp::from_secs(2);
    assert!(a < b);
}

#[test]
fn arithmetic() {
    let ts = Timestamp::from_secs(10);
    assert_eq!(ts.saturating_add(1_000_000), Timestamp::from_secs(11));
    assert_eq!(ts.saturating_sub(5_000_000), Timestamp::from_secs(5));
    assert_eq!(Timestamp::from_secs(0).saturating_sub(1), Timestamp::EPOCH);
}

#[test]
fn duration_since() {
    let a = Timestamp::from_secs(10);
    let b = Timestamp::from_secs(15);
    assert_eq!(b.duration_since(a), 5_000_000);
}
