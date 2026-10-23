#[path = "audit_success_rows_cut_tests/fixture.rs"]
mod fixture;
#[path = "audit_success_rows_cut_tests/obligation_tests.rs"]
mod obligation_tests;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::AuditSuccessRowsMode::{All, Reduced};
use crate::usage::{scope, UsageContext, UsageUnit};
use fixture::*;
use serde_json::{json, Value};

pub(super) fn observe_allocation(verb: &str, seq: u64) {
    fixture::observe(verb, seq);
}
fn selector(name: &str) -> String {
    format!("pack::audit_success_rows_cut_tests::{name}")
}
async fn finish(rig: &Rig) {
    rig.registry.shutdown_audit_batch().await.unwrap();
}

#[tokio::test]
async fn audit_success_rows_omits_exact_trusted_poll_successes() {
    let Some(case) = isolate(
        &selector("audit_success_rows_omits_exact_trusted_poll_successes"),
        &["ordinary", "intercepted"],
    ) else {
        return;
    };
    let rig = rig(Reduced, true, true);
    let admission = (
        super::audit_admission_refused_obligation_count(),
        super::audit_admission_unresolved_obligation_count(),
        super::audit_retry_exhausted_obligation_count(),
    );
    for (index, verb) in OMITTED.iter().enumerate() {
        let before = counts(&rig.registry);
        assert_eq!(
            call(&rig, verb, index as u64, case == "intercepted")
                .await
                .unwrap(),
            json!({"ok":true})
        );
        let after = counts(&rig.registry);
        for name in OMITTED {
            assert_eq!(
                count(&after.0, name) - count(&before.0, name),
                u64::from(name == *verb)
            );
        }
        assert_eq!((after.1, after.2), (before.1, before.2));
    }
    finish(&rig).await;
    assert_eq!(rig.control.calls.load(Ordering::Relaxed), 4);
    assert!(rig.store.rows().is_empty());
    assert_eq!(rig.store.submitted.load(Ordering::Relaxed), 0);
    assert!(allocations().is_empty());
    assert_eq!(
        (
            super::audit_admission_refused_obligation_count(),
            super::audit_admission_unresolved_obligation_count(),
            super::audit_retry_exhausted_obligation_count()
        ),
        admission
    );
}

#[tokio::test]
async fn audit_success_rows_registration_boundary_keeps_untrusted_successes() {
    let Some(case) = isolate(
        &selector("audit_success_rows_registration_boundary_keeps_untrusted_successes"),
        &["ordinary", "intercepted"],
    ) else {
        return;
    };
    let untrusted = rig(Reduced, false, true);
    for index in 0..20 {
        call(&untrusted, "comm.inbox", index, case == "intercepted")
            .await
            .unwrap();
        call(&untrusted, HEARTBEAT, 100 + index, case == "intercepted")
            .await
            .unwrap();
    }
    finish(&untrusted).await;
    assert_eq!(untrusted.store.rows().len(), 40);
    assert!(untrusted
        .store
        .rows()
        .iter()
        .all(|row| sampling(row).is_null()));
    assert!(allocations().is_empty());
    assert!(counts(&untrusted.registry)
        .0
        .iter()
        .chain(counts(&untrusted.registry).1.iter())
        .all(|(_, n)| *n == 0));
    let trusted = rig(Reduced, true, true);
    call(&trusted, HEARTBEAT, 200, case == "intercepted")
        .await
        .unwrap();
    call(&trusted, "stats", 201, case == "intercepted")
        .await
        .unwrap();
    finish(&trusted).await;
    assert_eq!(sampling(&trusted.store.rows()[0])["seq"], 0);
    assert_eq!(trusted.store.rows().len(), 2);
    for shadow_trusted in [false, true] {
        let shadow = Arc::new(Control::default());
        let canonical = Arc::new(Control::default());
        let store = Arc::new(Store::default());
        let mut builder = super::VerbRegistryBuilder::new();
        builder.with_audit_success_rows_mode(Reduced);
        let pack = PolicyPack {
            owner: "extension",
            handlers: COMM,
            control: shadow.clone(),
        };
        if shadow_trusted {
            builder.register_trusted(pack);
        } else {
            builder.register(pack);
        }
        builder.register_trusted(PolicyPack {
            owner: "comm",
            handlers: COMM,
            control: canonical.clone(),
        });
        builder.with_event_store(store.clone());
        let shadow_rig = Rig {
            registry: builder.build().unwrap(),
            store,
            control: shadow.clone(),
        };
        let before = allocations().len();
        call(
            &shadow_rig,
            HEARTBEAT,
            300 + u64::from(shadow_trusted),
            false,
        )
        .await
        .unwrap();
        finish(&shadow_rig).await;
        assert_eq!(shadow.calls.load(Ordering::Relaxed), 1);
        assert_eq!(canonical.calls.load(Ordering::Relaxed), 0);
        assert_eq!(shadow_rig.store.rows().len(), 1);
        assert!(sampling(&shadow_rig.store.rows()[0]).is_null());
        assert_eq!(allocations().len(), before);
    }
}

#[tokio::test]
async fn audit_success_rows_splitmix_serial_and_restart() {
    let Some(case) = isolate(
        &selector("audit_success_rows_splitmix_serial_and_restart"),
        &["serial", "restart"],
    ) else {
        return;
    };
    let rig = rig(Reduced, true, true);
    let calls = if case == "serial" { 100 } else { 1 };
    for n in 0..calls {
        let context = UsageContext::new();
        scope(context, call(&rig, HEARTBEAT, 1000 + n, false))
            .await
            .unwrap();
    }
    finish(&rig).await;
    let rows = rig.store.rows();
    let seqs: Vec<_> = rows
        .iter()
        .map(|row| sampling(row)["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(
        seqs,
        if calls == 100 {
            vec![0, 2, 8, 12, 34, 47, 77, 90]
        } else {
            vec![0]
        }
    );
    for row in &rows {
        let n = sampling(row)["seq"].as_u64().unwrap();
        assert_eq!(
            sampling(row),
            &json!({"every":10,"seq":n,"selected_by":"sample"})
        );
        assert_eq!(row.payload["resource"]["request_id"], 1000 + n);
        assert_eq!(row.payload["resource"]["cost_unit"], 1);
        assert_eq!(
            row.payload["resource"]["units"],
            json!({"db_round_trips":3})
        );
        assert!(row.duration_us >= 0);
    }
    assert_eq!(
        count(&counts(&rig.registry).1, HEARTBEAT),
        if calls == 100 { 92 } else { 0 }
    );
    assert_eq!(allocations().len(), calls as usize);
}

// Independently frozen from the ADR finalizer, never computed by the selector under test.
const SELECTED_8000: [u64; 775] = [
    0, 2, 8, 12, 34, 47, 77, 90, 110, 113, 115, 117, 125, 147, 152, 174, 188, 200, 203, 209, 211,
    214, 228, 234, 242, 245, 248, 253, 259, 267, 276, 279, 300, 329, 341, 343, 373, 380, 381, 386,
    415, 420, 440, 455, 456, 458, 459, 461, 466, 489, 495, 505, 515, 518, 524, 528, 549, 551, 559,
    576, 595, 624, 639, 646, 650, 661, 683, 686, 690, 725, 736, 755, 760, 762, 768, 776, 842, 861,
    880, 885, 895, 901, 914, 915, 925, 937, 947, 952, 954, 966, 978, 1003, 1009, 1016, 1025, 1036,
    1055, 1063, 1074, 1083, 1099, 1107, 1108, 1113, 1117, 1138, 1143, 1149, 1177, 1187, 1196, 1224,
    1226, 1238, 1240, 1244, 1246, 1267, 1269, 1277, 1286, 1287, 1299, 1334, 1360, 1369, 1374, 1376,
    1381, 1399, 1402, 1412, 1417, 1425, 1430, 1442, 1447, 1452, 1466, 1470, 1480, 1481, 1505, 1509,
    1521, 1555, 1578, 1589, 1608, 1621, 1626, 1628, 1631, 1642, 1654, 1655, 1658, 1672, 1678, 1681,
    1691, 1703, 1704, 1707, 1719, 1724, 1727, 1740, 1750, 1758, 1760, 1770, 1789, 1802, 1813, 1825,
    1846, 1857, 1863, 1871, 1874, 1891, 1894, 1895, 1908, 1913, 1914, 1918, 1923, 1937, 1950, 1955,
    1958, 1965, 1972, 1980, 1984, 2013, 2017, 2018, 2021, 2029, 2030, 2033, 2054, 2061, 2070, 2086,
    2100, 2101, 2111, 2116, 2120, 2141, 2148, 2149, 2171, 2193, 2196, 2206, 2213, 2217, 2233, 2236,
    2256, 2257, 2267, 2268, 2280, 2282, 2305, 2306, 2328, 2342, 2347, 2358, 2363, 2368, 2385, 2402,
    2431, 2434, 2438, 2444, 2452, 2453, 2457, 2465, 2466, 2495, 2513, 2515, 2520, 2543, 2554, 2563,
    2565, 2581, 2595, 2602, 2611, 2621, 2629, 2633, 2639, 2663, 2679, 2684, 2705, 2714, 2716, 2722,
    2741, 2745, 2747, 2750, 2755, 2767, 2775, 2790, 2805, 2817, 2832, 2837, 2838, 2847, 2851, 2868,
    2895, 2911, 2912, 2913, 2930, 2936, 2937, 2938, 2942, 2959, 2960, 2967, 2985, 3022, 3056, 3057,
    3078, 3086, 3090, 3111, 3129, 3130, 3147, 3155, 3161, 3179, 3180, 3181, 3194, 3211, 3216, 3218,
    3220, 3223, 3231, 3233, 3238, 3246, 3249, 3264, 3274, 3283, 3284, 3290, 3298, 3317, 3326, 3342,
    3375, 3386, 3425, 3454, 3455, 3457, 3465, 3468, 3479, 3502, 3504, 3516, 3538, 3552, 3559, 3566,
    3570, 3593, 3609, 3613, 3621, 3624, 3659, 3663, 3666, 3670, 3675, 3680, 3685, 3686, 3700, 3704,
    3713, 3727, 3734, 3753, 3760, 3761, 3802, 3806, 3816, 3818, 3823, 3824, 3858, 3881, 3887, 3903,
    3908, 3920, 3923, 3932, 3939, 3941, 3958, 3959, 3961, 3963, 3973, 3981, 3985, 3994, 3997, 3999,
    4012, 4032, 4037, 4039, 4044, 4051, 4058, 4069, 4102, 4130, 4139, 4172, 4189, 4202, 4203, 4206,
    4208, 4225, 4232, 4241, 4253, 4255, 4265, 4275, 4286, 4289, 4290, 4292, 4356, 4371, 4377, 4394,
    4398, 4439, 4465, 4471, 4474, 4492, 4493, 4497, 4499, 4503, 4524, 4530, 4537, 4542, 4571, 4583,
    4596, 4607, 4623, 4651, 4660, 4695, 4745, 4746, 4767, 4770, 4776, 4794, 4812, 4813, 4825, 4832,
    4840, 4862, 4872, 4876, 4879, 4904, 4906, 4908, 4910, 4913, 4914, 4952, 4959, 4962, 4965, 4974,
    4978, 4983, 5003, 5009, 5031, 5033, 5056, 5058, 5060, 5061, 5080, 5086, 5105, 5110, 5124, 5137,
    5139, 5141, 5143, 5165, 5173, 5181, 5188, 5191, 5195, 5209, 5210, 5213, 5215, 5228, 5235, 5237,
    5241, 5243, 5258, 5262, 5264, 5270, 5278, 5288, 5294, 5307, 5316, 5333, 5334, 5337, 5339, 5355,
    5370, 5383, 5384, 5391, 5398, 5400, 5433, 5441, 5451, 5453, 5510, 5519, 5558, 5562, 5584, 5590,
    5594, 5623, 5647, 5649, 5665, 5670, 5696, 5701, 5704, 5709, 5757, 5761, 5787, 5815, 5820, 5825,
    5826, 5847, 5856, 5858, 5859, 5863, 5890, 5898, 5905, 5923, 5925, 5933, 5938, 5942, 6007, 6017,
    6035, 6060, 6072, 6074, 6094, 6099, 6104, 6111, 6113, 6114, 6141, 6156, 6163, 6191, 6240, 6247,
    6251, 6254, 6261, 6275, 6281, 6292, 6299, 6304, 6310, 6324, 6329, 6330, 6335, 6336, 6376, 6381,
    6389, 6394, 6403, 6416, 6420, 6434, 6435, 6461, 6480, 6492, 6501, 6503, 6504, 6526, 6541, 6544,
    6547, 6548, 6560, 6561, 6567, 6591, 6599, 6612, 6635, 6646, 6656, 6657, 6670, 6683, 6700, 6736,
    6742, 6743, 6755, 6756, 6770, 6781, 6790, 6799, 6809, 6824, 6831, 6850, 6851, 6853, 6857, 6866,
    6876, 6892, 6911, 6916, 6920, 6925, 6926, 6931, 6947, 6948, 6961, 6964, 6965, 7001, 7004, 7025,
    7036, 7048, 7058, 7063, 7080, 7081, 7082, 7084, 7087, 7099, 7105, 7116, 7126, 7128, 7137, 7141,
    7142, 7161, 7170, 7212, 7224, 7228, 7244, 7249, 7266, 7271, 7277, 7294, 7303, 7305, 7308, 7313,
    7326, 7359, 7368, 7378, 7381, 7390, 7391, 7398, 7419, 7423, 7466, 7467, 7491, 7499, 7512, 7539,
    7549, 7553, 7559, 7561, 7567, 7574, 7585, 7594, 7612, 7614, 7619, 7622, 7624, 7627, 7631, 7645,
    7650, 7652, 7664, 7671, 7677, 7680, 7683, 7691, 7694, 7698, 7740, 7747, 7750, 7763, 7783, 7785,
    7786, 7796, 7809, 7811, 7812, 7815, 7830, 7839, 7842, 7847, 7851, 7880, 7895, 7902, 7917, 7938,
    7949, 7953, 7954, 7974, 7989, 7991, 7993,
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_success_rows_concurrent_sequence_is_unique() {
    let Some(_) = isolate(
        &selector("audit_success_rows_concurrent_sequence_is_unique"),
        &["run"],
    ) else {
        return;
    };
    let rig = Arc::new(rig(Reduced, true, true));
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for worker in 0..8u64 {
        let rig = rig.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            for n in 0..1000u64 {
                call_in(
                    &rig,
                    CLEANUP,
                    worker * 1000 + n,
                    false,
                    if worker % 2 == 0 { "local" } else { "parallel" },
                )
                .await
                .unwrap();
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    finish(&rig).await;
    let observed = allocations();
    assert_eq!(observed.len(), 8000);
    let numbers: BTreeSet<_> = observed.iter().map(|allocation| allocation.seq).collect();
    assert_eq!(
        numbers,
        (0..8000).collect(),
        "every allocation must be unique, including skipped calls"
    );
    let by_request: BTreeMap<_, _> = observed
        .iter()
        .map(|allocation| (allocation.request, allocation.seq))
        .collect();
    assert_eq!(by_request.len(), 8000);
    let mut kept = Vec::new();
    for row in rig.store.rows() {
        let request = row.payload["resource"]["request_id"].as_u64().unwrap();
        let seq = sampling(&row)["seq"].as_u64().unwrap();
        assert_eq!(by_request[&request], seq);
        assert_eq!(sampling(&row)["selected_by"], "sample");
        kept.push(seq);
    }
    kept.sort_unstable();
    assert_eq!(kept, SELECTED_8000);
    assert_eq!(count(&counts(&rig.registry).1, CLEANUP), 7225);
    assert_eq!(count(&counts(&rig.registry).2, CLEANUP), 0);
}

#[tokio::test]
async fn audit_success_rows_forcing_is_fail_closed() {
    let cases = [
        "heart_failure",
        "heart_missing",
        "heart_null",
        "heart_number",
        "heart_nonobject",
        "heart_clean",
        "heart_overlap",
        "cleanup_deleted_one",
        "cleanup_deleted_missing",
        "cleanup_deleted_null",
        "cleanup_deleted_string",
        "cleanup_deleted_negative",
        "cleanup_deleted_fraction",
        "cleanup_deleted_float_zero",
        "cleanup_deleted_boolean",
        "cleanup_detached_one",
        "cleanup_detached_missing",
        "cleanup_detached_null",
        "cleanup_detached_string",
        "cleanup_detached_negative",
        "cleanup_detached_fraction",
        "cleanup_detached_float_zero",
        "cleanup_detached_boolean",
        "cleanup_nonobject",
        "cleanup_clean",
        "cleanup_overlap",
    ];
    let Some(case) = isolate(
        &selector("audit_success_rows_forcing_is_fail_closed"),
        &cases,
    ) else {
        return;
    };
    let verb = if case.starts_with("heart_") {
        HEARTBEAT
    } else {
        CLEANUP
    };
    let rig = rig(Reduced, true, true);
    let overlap = case.ends_with("overlap");
    if !overlap {
        call(&rig, verb, 0, false).await.unwrap();
    }
    let mut value = clean(verb);
    if case == "heart_failure" || case == "heart_overlap" {
        value["outcome"] = json!("failure");
    } else if case == "heart_missing" {
        value.as_object_mut().unwrap().remove("outcome");
    } else if case == "heart_null" {
        value["outcome"] = Value::Null;
    } else if case == "heart_number" {
        value["outcome"] = json!(1);
    } else if case.ends_with("nonobject") {
        value = Value::Null;
    } else if case.starts_with("cleanup_") && !case.ends_with("clean") {
        let field = if case.starts_with("cleanup_detached_") {
            "routed_owners_detached"
        } else {
            "deleted"
        };
        if case.ends_with("missing") {
            value.as_object_mut().unwrap().remove(field);
        } else {
            value[field] = if case.ends_with("null") {
                Value::Null
            } else if case.ends_with("string") {
                json!("1")
            } else if case.ends_with("negative") {
                json!(-1)
            } else if case.ends_with("fraction") {
                json!(0.5)
            } else if case.ends_with("float_zero") {
                json!(0.0)
            } else if case.ends_with("boolean") {
                json!(false)
            } else {
                json!(1)
            };
        }
    }
    *rig.control.result.write().unwrap() = Some(value);
    call(&rig, verb, 1, false).await.unwrap();
    finish(&rig).await;
    let forced = !case.ends_with("clean") && !overlap;
    let rows = rig.store.rows();
    assert_eq!(rows.len(), if forced { 2 } else { 1 });
    if forced {
        assert_eq!(
            sampling(rows.last().unwrap()),
            &json!({"every":10,"seq":1,"selected_by":"forced"})
        );
    }
    if overlap {
        assert_eq!(
            sampling(&rows[0]),
            &json!({"every":10,"seq":0,"selected_by":"sample"})
        );
    }
    assert_eq!(count(&counts(&rig.registry).2, verb), u64::from(forced));
    assert_eq!(
        count(&counts(&rig.registry).1, verb),
        u64::from(case.ends_with("clean"))
    );
}

#[tokio::test]
async fn audit_success_rows_preserves_non_success_audits() {
    let Some(case) = isolate(
        &selector("audit_success_rows_preserves_non_success_audits"),
        &["ordinary", "intercepted"],
    ) else {
        return;
    };
    let rig = rig(Reduced, true, true);
    rig.control.error.store(true, Ordering::SeqCst);
    for (index, verb) in OMITTED.into_iter().chain([HEARTBEAT, CLEANUP]).enumerate() {
        let error = call(&rig, verb, index as u64, case == "intercepted")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("fixture domain error"));
        let rows = rig.store.rows();
        assert_eq!(
            rows.len(),
            index + 1,
            "non-success audits must never be cut"
        );
        assert_eq!(
            rows.last().unwrap().outcome,
            khive_types::EventOutcome::Error
        );
        assert!(sampling(rows.last().unwrap()).is_null());
        assert!(rows.last().unwrap().payload.get("error_class").is_some());
    }
    assert!(allocations().is_empty());
    assert!(counts(&rig.registry).0.iter().all(|(_, n)| *n == 0));
    rig.control.error.store(false, Ordering::SeqCst);
    call(&rig, HEARTBEAT, 20, case == "intercepted")
        .await
        .unwrap();
    finish(&rig).await;
    assert_eq!(sampling(rig.store.rows().last().unwrap())["seq"], 0);
}

#[tokio::test]
async fn audit_success_rows_storeless_dispatch_does_not_cut() {
    let Some(case) = isolate(
        &selector("audit_success_rows_storeless_dispatch_does_not_cut"),
        &["ordinary", "intercepted"],
    ) else {
        return;
    };
    let empty = rig(Reduced, true, false);
    let before = counts(&empty.registry);
    for (index, verb) in OMITTED
        .into_iter()
        .chain([HEARTBEAT, CLEANUP, HEARTBEAT, CLEANUP])
        .enumerate()
    {
        if index >= 6 {
            *empty.control.result.write().unwrap() = Some(Value::Null);
        }
        let context = UsageContext::new();
        scope(
            context.clone(),
            call(&empty, verb, index as u64, case == "intercepted"),
        )
        .await
        .unwrap();
        context.add(UsageUnit::GraphHops, 7);
        assert_eq!(
            context.shipping_snapshot().unwrap()["graph_hops"],
            7,
            "no store must leave usage unfrozen"
        );
    }
    assert_eq!(counts(&empty.registry), before);
    assert!(allocations().is_empty());
    let backed = rig(Reduced, true, true);
    for (index, verb) in [HEARTBEAT, CLEANUP].into_iter().enumerate() {
        call(&backed, verb, 20 + index as u64, case == "intercepted")
            .await
            .unwrap();
    }
    finish(&backed).await;
    assert!(backed
        .store
        .rows()
        .iter()
        .all(|row| sampling(row)["seq"] == 0));
    let mut direct = rig(Reduced, true, true);
    direct.registry.audit_batch = None;
    call(&direct, HEARTBEAT, 30, case == "intercepted")
        .await
        .unwrap();
    call(&direct, HEARTBEAT, 31, case == "intercepted")
        .await
        .unwrap();
    assert_eq!(direct.store.rows().len(), 1);
    assert_eq!(sampling(&direct.store.rows()[0])["seq"], 2);
    call(&direct, "comm.inbox", 32, case == "intercepted")
        .await
        .unwrap();
    assert_eq!(count(&counts(&direct.registry).0, "comm.inbox"), 1);
}

#[tokio::test]
async fn audit_success_rows_replay_reduces_fixture_rows() {
    let Some(case) = isolate(
        &selector("audit_success_rows_replay_reduces_fixture_rows"),
        &["all", "reduced"],
    ) else {
        return;
    };
    let rig = rig(if case == "all" { All } else { Reduced }, true, true);
    let mut request = 0;
    for (verb, calls) in [
        ("comm.inbox", 355),
        (CLEANUP, 253),
        ("whoami", 250),
        ("comm.probe", 247),
        ("comm.cursor_get", 127),
        (HEARTBEAT, 127),
        ("stats", 290),
    ] {
        for _ in 0..calls {
            call(&rig, verb, request, false).await.unwrap();
            request += 1;
        }
    }
    finish(&rig).await;
    assert_eq!(request, 1649);
    assert_eq!(rig.control.calls.load(Ordering::Relaxed), 1649);
    let rows = rig.store.rows();
    assert_eq!(rows.len(), if case == "all" { 1649 } else { 330 });
    if case == "reduced" {
        assert_eq!(rows.iter().filter(|row| row.verb == CLEANUP).count(), 27);
        assert_eq!(rows.iter().filter(|row| row.verb == HEARTBEAT).count(), 13);
        assert_eq!(rows.iter().filter(|row| row.verb == "stats").count(), 290);
    } else {
        assert!(allocations().is_empty());
        assert!(rows.iter().all(|row| sampling(row).is_null()));
        assert!(counts(&rig.registry)
            .0
            .iter()
            .chain(counts(&rig.registry).1.iter())
            .all(|(_, n)| *n == 0));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_success_rows_two_registries_share_sequences_not_mode() {
    let Some(_) = isolate(
        &selector("audit_success_rows_two_registries_share_sequences_not_mode"),
        &["run"],
    ) else {
        return;
    };
    let first = Arc::new(rig(Reduced, true, true));
    let second = Arc::new(rig(Reduced, true, true));
    let all = rig(All, true, true);
    call(&first, HEARTBEAT, 0, false).await.unwrap();
    call(&all, HEARTBEAT, 100, false).await.unwrap();
    call(&second, HEARTBEAT, 1, false).await.unwrap();
    let mut tasks = Vec::new();
    for n in 2..22 {
        let registry = if n % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        tasks.push(tokio::spawn(async move {
            call_in(
                &registry,
                HEARTBEAT,
                n,
                false,
                if n % 2 == 0 { "local" } else { "parallel" },
            )
            .await
            .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    finish(&first).await;
    finish(&second).await;
    finish(&all).await;
    assert_eq!(first.registry.audit_success_rows_mode(), Reduced);
    assert_eq!(second.registry.audit_success_rows_mode(), Reduced);
    assert_eq!(all.registry.audit_success_rows_mode(), All);
    let observed = allocations();
    assert_eq!(observed.len(), 22);
    assert_eq!(
        observed.iter().map(|a| a.seq).collect::<BTreeSet<_>>(),
        (0..22).collect()
    );
    let by_request: BTreeMap<_, _> = observed.iter().map(|a| (a.request, a.seq)).collect();
    for row in first.store.rows().into_iter().chain(second.store.rows()) {
        let request = row.payload["resource"]["request_id"].as_u64().unwrap();
        assert_eq!(sampling(&row)["seq"], by_request[&request]);
    }
    assert_eq!(all.store.rows().len(), 1);
    assert!(sampling(&all.store.rows()[0]).is_null());
    assert_eq!(counts(&first.registry), counts(&second.registry));
    assert_eq!(counts(&first.registry), counts(&all.registry));
    let later = rig(Reduced, true, true);
    assert_eq!(counts(&later.registry), counts(&first.registry));
    finish(&later).await;
}

#[derive(Debug)]
struct DenyGate;
impl khive_gate::Gate for DenyGate {
    fn check(
        &self,
        _request: &khive_gate::GateRequest,
    ) -> Result<khive_gate::GateDecision, khive_gate::GateError> {
        Ok(khive_gate::GateDecision::deny(
            "audit fixture policy denial",
        ))
    }
}

#[tokio::test]
async fn audit_success_rows_denial_and_unknown_keep_receipts() {
    let Some(case) = isolate(
        &selector("audit_success_rows_denial_and_unknown_keep_receipts"),
        &["ordinary", "intercepted"],
    ) else {
        return;
    };
    let mut rig = rig(Reduced, true, true);
    rig.registry.gate = Arc::new(DenyGate);
    let before = counts(&rig.registry);
    let error = call(&rig, "whoami", 1, case == "intercepted")
        .await
        .unwrap_err();
    assert_eq!(
        error.disposition(),
        crate::error::DomainDisposition::NotCommitted
    );
    let crate::RuntimeError::PermissionDenied { receipt, .. } = error.into_source() else {
        panic!("policy denial must stay distinct");
    };
    let rows = rig.store.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(receipt.audit_event_id, Some(rows[0].id));
    assert_eq!(rows[0].outcome, khive_types::EventOutcome::Denied);
    assert!(sampling(&rows[0]).is_null());
    rig.registry.gate = Arc::new(khive_gate::AllowAllGate);
    let error = call(&rig, "success_fixture.unknown", 2, false)
        .await
        .unwrap_err();
    assert!(matches!(
        error.into_source(),
        crate::RuntimeError::UnknownVerb(_)
    ));
    assert_eq!(rig.store.rows().len(), 2);
    assert!(rig.store.rows().iter().all(|row| sampling(row).is_null()));
    assert!(allocations().is_empty());
    assert_eq!(counts(&rig.registry), before);
    call(&rig, HEARTBEAT, 3, case == "intercepted")
        .await
        .unwrap();
    finish(&rig).await;
    assert_eq!(sampling(rig.store.rows().last().unwrap())["seq"], 0);
}

#[tokio::test]
async fn audit_success_rows_dispatch_mode_is_registry_scoped() {
    let Some(case) = isolate(
        &selector("audit_success_rows_dispatch_mode_is_registry_scoped"),
        &["fixed", "override_all", "override_reduced"],
    ) else {
        return;
    };
    if case == "fixed" {
        std::env::set_var("KHIVE_AUDIT_SUCCESS_ROWS", "reduced");
        let old = env_rig();
        std::env::set_var("KHIVE_AUDIT_SUCCESS_ROWS", "all");
        let new = env_rig();
        call(&old, HEARTBEAT, 0, false).await.unwrap();
        let before_all = counts(&old.registry);
        call(&new, HEARTBEAT, 100, false).await.unwrap();
        assert_eq!(counts(&new.registry), before_all);
        call(&old, HEARTBEAT, 1, false).await.unwrap();
        finish(&old).await;
        finish(&new).await;
        assert_eq!(old.registry.audit_success_rows_mode(), Reduced);
        assert_eq!(new.registry.audit_success_rows_mode(), All);
        assert_eq!(
            allocations().iter().map(|a| a.seq).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(old.store.rows().len(), 1);
        assert_eq!(sampling(&old.store.rows()[0])["seq"], 0);
        assert_eq!(new.store.rows().len(), 1);
        assert!(sampling(&new.store.rows()[0]).is_null());
    } else {
        let all = case == "override_all";
        std::env::set_var(
            "KHIVE_AUDIT_SUCCESS_ROWS",
            if all { "reduced" } else { "all" },
        );
        let rig = rig(if all { All } else { Reduced }, true, true);
        call(&rig, "comm.inbox", 1, false).await.unwrap();
        finish(&rig).await;
        assert_eq!(rig.store.rows().len(), usize::from(all));
        assert_eq!(
            count(&counts(&rig.registry).0, "comm.inbox"),
            u64::from(!all)
        );
        assert!(allocations().is_empty());
    }
}
