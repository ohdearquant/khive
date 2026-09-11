use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::mount_config::MountEffect;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MountedVerb {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub effect: MountEffect,
    pub digest: String,
    pub generation: i64,
}

impl MountedVerb {
    pub fn describe(&self, pack: &str) -> Value {
        json!({
            "verb": format!("{pack}.{}", self.name), "pack": pack,
            "description": self.description.as_deref().unwrap_or("Mounted tool"),
            "visibility": "Verb",
            "signature": format!("{pack}.{}(...)", self.name),
            "category": if self.effect == MountEffect::Read { "Assertive" } else { "Commissive" },
            "input_schema": self.input_schema, "output_schema": self.output_schema,
            "effect": self.effect, "generation": self.generation,
        })
    }
}
