use serde_json::Value;

use khive_runtime::{NamespaceToken, RuntimeResult};
use khive_types::{HandlerDef, IdResolutionMode, ParamDef, VerbCategory, Visibility};

use crate::BrainPack;

pub(super) const HANDLER: HandlerDef = HandlerDef {
    name: "brain.event_page",
    description: "Page stored event rows and payloads in ascending timestamp/ID order. The cursor \
        is a live-window position, not a snapshot: only greater stored keys can appear later. \
        Current and historical GTD audit rows lack task ID and prior/new status; no fields are \
        inferred.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[
        ParamDef {
            name: "since",
            param_type: "string",
            required: true,
            description: "Inclusive window start, RFC3339 or a date in the display timezone.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "until",
            param_type: "string",
            required: false,
            description: "Exclusive window end. First omission freezes now; continuation \
                omission keeps that bound.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "kind",
            param_type: "string",
            required: false,
            description: "One EventKind; combined with kinds and deduplicated.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "kinds",
            param_type: "array",
            required: false,
            description: "EventKind names; omit for all kinds.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "namespaces",
            param_type: "array",
            required: false,
            description: "At most 16 requested namespaces, intersected with current visibility. \
                Default is the request namespace; [] is empty.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "exclude_namespaces",
            param_type: "array",
            required: false,
            description: "At most 32 namespaces excluded in the query before paging.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "actor",
            param_type: "string",
            required: false,
            description: "Same actor aliases and visibility policy as brain.event_counts; \
                default is the caller.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "all_actors",
            param_type: "boolean",
            required: false,
            description: "Requires the serving brain.fleet_readers allowlist; cannot be \
                combined with actor and does not widen namespaces.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "limit",
            param_type: "integer",
            required: false,
            description: "Page size 1 through 1000; default 100.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "after",
            param_type: "string",
            required: false,
            description: "Opaque continuation from next_after; filters, principal and current \
                visibility must still match.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
    ],
};

impl BrainPack {
    pub(crate) async fn handle_event_page(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> RuntimeResult<Value> {
        self.runtime.read_event_page_response(token, params).await
    }
}
