use super::*;
use serde_json::json;

#[test]
fn issue_2537_standard_context_baseline_controls() {
    let timestamp = "2026-01-01T00:00:00.123456Z";
    let now = 1_767_225_780; // 2026-01-01T00:03:00Z, independently literal clock.
    let uuid = "aabbccdd-1234-4321-1234-abcdefabcdef";
    let payload = json!({"id":uuid,"created_at":timestamp,"empty":[],"null":null});
    let value = json!({"tool":"stream.batch","policy":"StreamBatchReceipts","id":uuid,
            "updated_at":timestamp,"score":0.123456,"empty":[],"null":null,
            "results":[{"id":uuid,"version":1,"updated_at":timestamp,"details":{"updated_at":timestamp}}],
            "properties":{"created_at":timestamp,"id":uuid,"empty":""},"trigger_at":timestamp,"due":timestamp,
            "entry":{"seq":1,"id":uuid,"created_at":timestamp,"record":payload},
            "cursor":{"head_seq":0,"entries":[],"next_after":null}});
    let shown = present(value.clone(), PresentationMode::Agent, now);
    assert_eq!(shown["updated_at"], timestamp);
    assert_eq!(shown["results"][0]["updated_at"], timestamp);
    assert_eq!(shown["results"][0]["details"]["updated_at"], timestamp);
    assert_eq!(shown["id"], "aabbccdd");
    assert_eq!(shown["score"], json!(0.123));
    assert!(shown.get("empty").is_none());
    assert!(shown.get("null").is_none());
    assert_eq!(
        shown["properties"],
        json!({"created_at":timestamp,"id":"aabbccdd","empty":""})
    );
    assert_eq!(shown["trigger_at"], timestamp);
    assert_eq!(shown["due"], timestamp);
    assert_eq!(shown["entry"]["record"], payload);
    assert_eq!(shown["entry"]["created_at"], timestamp);
    assert_eq!(shown["cursor"], value["cursor"]);
    for mode in [PresentationMode::Verbose, PresentationMode::Human] {
        assert_eq!(present(value.clone(), mode, now), value);
    }
    assert_eq!(
        present(
            json!({"properties":timestamp}),
            PresentationMode::Agent,
            now
        )["properties"],
        timestamp
    );
    assert_eq!(
        present(
            json!({"properties":[timestamp,{"created_at":timestamp}]}),
            PresentationMode::Agent,
            now
        )["properties"],
        json!([timestamp,{"created_at":timestamp}])
    );
}
