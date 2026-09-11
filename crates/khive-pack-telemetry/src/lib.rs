//! Configured telemetry carriers and bounded queries over the stream substrate.

mod handlers;
mod pack;

use khive_runtime::KhiveRuntime;
use khive_types::{HandlerDef, Pack};

pub struct TelemetryPack {
    runtime: KhiveRuntime,
}

impl TelemetryPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
}

impl Pack for TelemetryPack {
    const NAME: &'static str = "telemetry";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &pack::TELEMETRY_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}
