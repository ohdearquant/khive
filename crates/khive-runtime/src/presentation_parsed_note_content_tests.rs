use super::*;
use serde_json::json;

#[test]
fn issue2757_content_is_opaque_to_metadata_and_format_reductions() {
    let payload = json!([
        {"id":"11111111-1111-4111-8111-111111111111", "full_id":"keep", "namespace":"local",
         "created_at":"2026-09-15T12:34:56.123456Z", "score":0.123456789,
         "nil":null, "empty":"", "array":[], "object":{},
         "properties":{"id":"11111111-1111-4111-8111-111111111111"}},
        {"status":"pending"}
    ]);
    for (scope, response, pointer) in [
        (
            NoteContentScope::Record,
            json!({"id":"22222222-2222-4222-8222-222222222222", "content":payload}),
            "/content",
        ),
        (
            NoteContentScope::Items,
            json!({"items":[{"id":"22222222-2222-4222-8222-222222222222", "content":payload}], "requested_limit":1, "effective_limit":1, "limit_clamped":false}),
            "/items/0/content",
        ),
        (
            NoteContentScope::Notes,
            json!({"notes":[{"id":"22222222-2222-4222-8222-222222222222", "content":payload}], "next_after":null}),
            "/notes/0/content",
        ),
    ] {
        let presented = scope.protect(response, |value| present(value, PresentationMode::Agent, 0));
        for format in [OutputFormat::Json, OutputFormat::Auto, OutputFormat::Table] {
            let rendered = render_format_with_note_content(
                presented.clone(),
                format,
                PresentationMode::Agent,
                scope,
            );
            let actual: Value = serde_json::from_str(&rendered)
                .expect("single note stays a JSON record, never a table of its body");
            let expected = if format == OutputFormat::Table {
                Value::String(payload.to_string())
            } else {
                payload.clone()
            };
            assert_eq!(actual.pointer(pointer), Some(&expected));
        }
    }
}
