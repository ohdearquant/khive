use async_trait::async_trait;
use khive_runtime::{KhiveRuntime, NamespaceToken, PackRuntime, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, IdResolutionMode, Pack, ParamDef, VerbCategory, Visibility};
use serde_json::Value;

use crate::{handlers, TelemetryPack};

pub(crate) static TELEMETRY_HANDLERS: [HandlerDef; 4] = [
    HandlerDef {
        name: "telemetry.channels",
        description: "Return the effective telemetry stream and carrier policies, including the \
                      required operator-declared default. Ephemeral events are \
                      accepted and dropped; no ring or reconnect gap history is retained.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    },
    HandlerDef {
        name: "telemetry.emit",
        description: "Route a generic JSON payload through the configured channel table. Durable \
                      events append to the stream; ephemeral events are accepted and dropped. \
                      Returns carrier, classified, actor and outcome (recorded/dropped/unknown). \
                      receipt_id names the stored stream row only for recorded outcomes and is null \
                      otherwise. Gap posture preserves the original structured append error; stop \
                      propagates it. Neither posture retries. Payloads have no per-kind schema.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "kind",
                param_type: "string",
                required: true,
                description: "Exact event kind to resolve against the channel table.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "payload",
                param_type: "JSON value",
                required: true,
                description: "Arbitrary JSON payload, including null; no per-kind validation.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "run_id",
                param_type: "string",
                required: false,
                description: "Optional opaque run identifier stored with a durable event.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Optional caller attribution; a different identity is ignored and \
                              reported by actor_argument_ignored=true. The resolved caller is stamped.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "telemetry.read",
        description: "Read one bounded durable stream page, scoped to the caller by default. Actor and kind filtering follow cursor \
                      advancement, so an empty filtered page can still advance next_cursor. \
                      Coverage labels the scanned window and current ephemeral-kind policy separately; \
                      historical durable rows remain readable after policy changes.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "stream",
                param_type: "string",
                required: true,
                description: "Arbitrary stream name, in the request namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "since",
                param_type: "integer",
                required: false,
                description: "Exclusive log sequence cursor, starting at 0; reuse next_cursor.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "limit",
                param_type: "integer",
                required: false,
                description: "Maximum stream rows scanned before filtering, 1..1000; default 100.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Optional visible actor to read; defaults to the calling actor.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "all_actors",
                param_type: "boolean",
                required: false,
                description: "Read all actors only for a configured brain.fleet_readers caller; cannot combine with actor.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "kinds",
                param_type: "array",
                required: false,
                description: "Optional nonempty list of up to 100 exact record.kind strings.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
    HandlerDef {
        name: "telemetry.counts",
        description: "Count an arbitrary durable stream in a half-open timestamp window, grouped \
                      by caller-selected record fields. A live read bounded by the initial head, \
                      not an atomic snapshot; concurrent hard deletions can change the population. \
                      complete means the bounded scan finished. Refuses scans over 50000 rows or results over 1000 groups \
                      instead of returning partial totals. Ephemeral drops are not counted.",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[
            ParamDef {
                name: "stream",
                param_type: "string",
                required: true,
                description: "Arbitrary stream name, in the request namespace.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "window",
                param_type: "object",
                required: true,
                description: "{since, until?} RFC3339 timestamps; since inclusive, until exclusive \
                              and defaults to now. Uses the stream row's created_at.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "group_by",
                param_type: "array",
                required: false,
                description: "1..8 unique dotted record fields, default [kind]; e.g. actor, \
                              payload.verb. Scalar values only; missing and null share a group. \
                              Paths are at most 128 bytes; a group key is at most 4096 bytes.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "actor",
                param_type: "string",
                required: false,
                description: "Optional visible actor to read; defaults to the calling actor.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "all_actors",
                param_type: "boolean",
                required: false,
                description: "Read all actors only for a configured brain.fleet_readers caller; cannot combine with actor.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
            ParamDef {
                name: "kinds",
                param_type: "array",
                required: false,
                description: "Optional nonempty list of up to 100 exact record.kind strings.",
                resolution_mode: IdResolutionMode::NotApplicable,
            },
        ],
    },
];

struct TelemetryPackFactory;

impl khive_runtime::PackFactory for TelemetryPackFactory {
    fn name(&self) -> &'static str {
        TelemetryPack::NAME
    }

    fn requires(&self) -> &'static [&'static str] {
        TelemetryPack::REQUIRES
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(TelemetryPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&TelemetryPackFactory) }

#[async_trait]
impl PackRuntime for TelemetryPack {
    fn validate_config(&self) -> Result<(), RuntimeError> {
        self.runtime
            .config()
            .telemetry
            .validate_activation()
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))
    }
    fn name(&self) -> &str {
        <Self as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <Self as Pack>::HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        <Self as Pack>::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.runtime
            .config()
            .telemetry
            .validate_activation()
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        match verb {
            "telemetry.channels" => handlers::channels(&self.runtime, params),
            "telemetry.emit" => handlers::emit(&self.runtime, token, params).await,
            "telemetry.read" => handlers::read(&self.runtime, token, params).await,
            "telemetry.counts" => handlers::counts(&self.runtime, token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "telemetry pack does not handle verb {verb:?}"
            ))),
        }
    }
}
