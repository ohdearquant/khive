use std::collections::BTreeMap;

use khive_storage::StorageError;
use khive_types::{Header, Namespace, PropertyValue, Timestamp};

fn invalid(field: &str) -> StorageError {
    StorageError::Internal(format!("canonical record cannot represent {field}"))
}

fn timestamp(value: i64, field: &str) -> Result<Timestamp, StorageError> {
    u64::try_from(value)
        .map(Timestamp::from_micros)
        .map_err(|_| invalid(field))
}

pub(super) fn properties(
    value: &Option<serde_json::Value>,
) -> Result<BTreeMap<String, PropertyValue>, StorageError> {
    match value {
        None => Ok(BTreeMap::new()),
        Some(serde_json::Value::Object(_)) => {
            serde_json::from_value(value.as_ref().expect("matched present properties").clone())
                .map_err(|_| invalid("properties"))
        }
        Some(_) => Err(invalid("properties")),
    }
}

pub(super) fn note(value: &khive_storage::Note) -> Result<khive_types::Note, StorageError> {
    let mut properties = properties(&value.properties)?;
    let tags = match properties.remove("tags") {
        None => Vec::new(),
        Some(PropertyValue::Array(tags)) => tags
            .into_iter()
            .map(|tag| match tag {
                PropertyValue::String(tag) => Ok(tag),
                _ => Err(invalid("note tags")),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(invalid("note tags")),
    };
    let note = khive_types::Note {
        header: Header {
            id: value.id.into(),
            namespace: Namespace::parse(&value.namespace).map_err(|_| invalid("namespace"))?,
            created_at: timestamp(value.created_at, "created_at")?,
            updated_at: timestamp(value.updated_at, "updated_at")?,
        },
        kind: value.kind.clone(),
        status: value.status.parse().map_err(|_| invalid("status"))?,
        content: value.content.clone(),
        properties,
        tags,
        salience: value.salience,
        decay_factor: value.decay_factor,
        expires_at: value
            .expires_at
            .map(|v| timestamp(v, "expires_at"))
            .transpose()?,
        deleted_at: value
            .deleted_at
            .map(|v| timestamp(v, "deleted_at"))
            .transpose()?,
    };
    if !note.is_valid() {
        return Err(invalid("note numeric fields"));
    }
    Ok(note)
}

pub(super) fn entity(value: &khive_storage::Entity) -> Result<khive_types::Entity, StorageError> {
    Ok(khive_types::Entity {
        header: Header {
            id: value.id.into(),
            namespace: Namespace::parse(&value.namespace).map_err(|_| invalid("namespace"))?,
            created_at: timestamp(value.created_at, "created_at")?,
            updated_at: timestamp(value.updated_at, "updated_at")?,
        },
        kind: value.kind.parse().map_err(|_| invalid("kind"))?,
        entity_type: value.entity_type.clone(),
        name: value.name.clone(),
        description: value.description.clone(),
        properties: properties(&value.properties)?,
        tags: value.tags.clone(),
        deleted_at: value
            .deleted_at
            .map(|v| timestamp(v, "deleted_at"))
            .transpose()?,
    })
}

pub(super) fn edge(value: &khive_storage::Edge) -> Result<khive_types::Link, StorageError> {
    let edge = khive_types::Link {
        id: uuid::Uuid::from(value.id).into(),
        namespace: value.namespace.clone(),
        source: value.source_id.into(),
        target: value.target_id.into(),
        relation: value.relation,
        properties: properties(&value.metadata)?,
        weight: value.weight,
        created_at: timestamp(value.created_at.timestamp_micros(), "created_at")?,
        updated_at: timestamp(value.updated_at.timestamp_micros(), "updated_at")?,
        deleted_at: value
            .deleted_at
            .map(|v| timestamp(v.timestamp_micros(), "deleted_at"))
            .transpose()?,
    };
    if !edge.is_valid() {
        return Err(invalid("weight"));
    }
    Ok(edge)
}
