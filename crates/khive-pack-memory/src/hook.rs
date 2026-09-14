//! `MemoryHook` — the memory pack's specialization for the `memory` note kind.
//!
//! `memory.remember` resolves `memory_type`, `salience` and `decay_factor` in
//! its own handler. Without this hook the generic `create(kind=memory)` path
//! stored a note with those columns null and no `memory_type` property, and
//! `memory.recall` filled the gaps at read time, so the same record read
//! differently by path. The hook runs the same resolution on `prepare_create`
//! and writes the resolved values back into the create arguments.

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, KindHook, RuntimeError};

use crate::handlers::common::{resolve_memory_defaults, MemoryDefaults};

#[derive(Debug, Default)]
/// KindHook implementation for the `memory` note kind; applies the remember defaults on create.
pub struct MemoryHook;

fn optional_number(args: &Value, name: &str) -> Result<Option<f64>, RuntimeError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_f64().map(Some).ok_or_else(|| {
            RuntimeError::InvalidInput(format!("{name} must be a finite number, got {n}"))
        }),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{name} must be a number, got {other}"
        ))),
    }
}

#[async_trait]
impl KindHook for MemoryHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let memory_type = match args.get("properties").and_then(|p| p.get("memory_type")) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(other) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "properties.memory_type must be a string, got {other}"
                )));
            }
        };
        let salience = optional_number(args, "salience")?;
        let decay_factor = optional_number(args, "decay_factor")?;
        let MemoryDefaults {
            memory_type,
            salience,
            decay_factor,
        } = resolve_memory_defaults(memory_type.as_deref(), salience, decay_factor)?;

        let obj = args.as_object_mut().ok_or_else(|| {
            RuntimeError::InvalidInput("create arguments must be an object".into())
        })?;
        obj.insert("salience".into(), json!(salience));
        obj.insert("decay_factor".into(), json!(decay_factor));
        let properties = obj
            .entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
        match properties {
            Value::Object(map) => {
                map.insert("memory_type".into(), json!(memory_type));
            }
            Value::Null => {
                *properties = json!({ "memory_type": memory_type });
            }
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "properties must be an object, got {other}"
                )));
            }
        }
        Ok(())
    }

    async fn after_create(
        &self,
        _runtime: &KhiveRuntime,
        _id: Uuid,
        _args: &Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}
