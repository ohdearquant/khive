use super::*;

#[test]
fn test_tombstone_stats_needs_rebuild() {
    let stats = TombstoneStats {
        total_nodes: 100,
        tombstone_count: 10,
        live_nodes: 90,
        ratio: 0.10,
    };
    assert!(!stats.needs_rebuild()); // 10% == 10% threshold (not strictly greater)

    let stats = TombstoneStats {
        total_nodes: 100,
        tombstone_count: 20,
        live_nodes: 80,
        ratio: 0.20,
    };
    assert!(stats.needs_rebuild()); // 20% > 10% threshold
}

#[test]
fn test_tombstone_stats_custom_threshold() {
    let stats = TombstoneStats {
        total_nodes: 100,
        tombstone_count: 10,
        live_nodes: 90,
        ratio: 0.10,
    };
    assert!(stats.needs_rebuild_at(0.05)); // 10% > 5%
    assert!(!stats.needs_rebuild_at(0.15)); // 10% < 15%
}
