use super::{QueryMailboxScope, QueryValue};

pub(super) struct MailboxLowering {
    actor_parameter: usize,
    maintainer_parameter: Option<usize>,
    legacy_local: bool,
}

impl MailboxLowering {
    pub(super) fn bind(scope: &QueryMailboxScope, params: &mut Vec<QueryValue>) -> Self {
        params.push(QueryValue::Text(scope.actor_id.clone()));
        let actor_parameter = params.len();
        let maintainer_parameter = scope
            .maintainer_actor
            .as_ref()
            .filter(|actor| !actor.is_empty())
            .map(|actor| {
                params.push(QueryValue::Text(actor.clone()));
                params.len()
            });
        Self {
            actor_parameter,
            maintainer_parameter,
            legacy_local: scope.legacy_local,
        }
    }

    pub(super) fn note_filter(&self, alias: &str) -> String {
        format!(" WHERE {}", self.note_visible(alias))
    }

    pub(super) fn edge_filter(&self, alias: &str) -> String {
        format!(
            " WHERE {} AND {}",
            self.endpoint_visible(&format!("{alias}.source_id")),
            self.endpoint_visible(&format!("{alias}.target_id"))
        )
    }

    fn endpoint_visible(&self, endpoint: &str) -> String {
        // Generic by-ID reads prefer entities (including tombstones) over notes.
        // NULL routing is a refusal, not an SQL UNKNOWN that escapes an anti-join.
        format!(
            "NOT EXISTS (SELECT 1 FROM notes mailbox_note \
             WHERE mailbox_note.id = {endpoint} AND mailbox_note.kind = 'message' \
             AND COALESCE({}, 0) = 0 \
             AND NOT EXISTS (SELECT 1 FROM entities mailbox_entity \
                             WHERE mailbox_entity.id = {endpoint}))",
            self.note_visible("mailbox_note")
        )
    }

    fn note_visible(&self, alias: &str) -> String {
        let json_type = |key: &str| format!("json_type({alias}.properties, '$.{key}')");
        let extract = |key: &str| format!("json_extract({alias}.properties, '$.{key}')");
        let absent = |key: &str| {
            let ty = json_type(key);
            format!("({ty} IS NULL OR {ty} = 'null')")
        };
        let not_text = |key: &str| format!("ifnull({}, '') != 'text'", json_type(key));
        let text_is_actor = |key: &str| {
            format!(
                "({} = 'text' AND {} = ?{})",
                json_type(key),
                extract(key),
                self.actor_parameter
            )
        };
        let direction = extract("direction");
        let to = text_is_actor("to_actor");
        let from = text_is_actor("from_actor");
        let (inbound_legacy, outbound_legacy, unrouted) = if self.legacy_local {
            (
                format!(" OR {}", absent("to_actor")),
                format!(" OR {}", absent("from_actor")),
                format!(
                    " OR ({} AND {} AND {})",
                    absent("direction"),
                    not_text("from_actor"),
                    not_text("to_actor")
                ),
            )
        } else {
            Default::default()
        };
        // Preserve MailboxView/NoteMailboxScope's typed-JSON legacy partition.
        let ordinary = format!(
            "({alias}.kind != 'message' OR ({direction} = 'inbound' AND ({to}{inbound_legacy})) \
             OR ({direction} = 'outbound' AND ({from}{outbound_legacy})){unrouted})"
        );
        let Some(maintainer) = self.maintainer_parameter else {
            return ordinary;
        };
        let quarantine_type = json_type("quarantined");
        let quarantine_value = extract("quarantined");
        format!(
            "({alias}.kind != 'message' OR CASE WHEN COALESCE(\
             {quarantine_type} = 'true' OR \
             ({quarantine_type} = 'text' AND {quarantine_value} = 'true'), 0) \
             THEN ?{} = ?{maintainer} ELSE COALESCE({ordinary}, 0) END)",
            self.actor_parameter
        )
    }
}
