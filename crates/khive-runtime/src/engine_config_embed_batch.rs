//! Validated `[embed]` declarations and legacy compatibility diagnostics.

use std::marker::PhantomData;
use std::path::Path;

use serde::de::IntoDeserializer;
use serde::Deserialize;

use super::{ConfigError, EngineConfig};

/// Parsed embedding policy. Hosts and producers apply each supported setting;
/// parsing a declaration alone does not enable its execution.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct EmbedSectionConfig {
    #[serde(default = "enabled")]
    pub auto_embed: bool,
    #[serde(default, deserialize_with = "positive_integer")]
    pub batch_size: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_fields")]
    pub fields: EntityEmbeddingFields,
    #[serde(default, deserialize_with = "legacy_model")]
    pub model: Option<String>,
    #[serde(default, deserialize_with = "positive_integer")]
    pub dimensions: Option<u64>,
    #[serde(default)]
    pub device: Option<LegacyEmbedDevice>,
}

fn enabled() -> bool {
    true
}

impl Default for EmbedSectionConfig {
    fn default() -> Self {
        Self {
            auto_embed: true,
            batch_size: None,
            fields: EntityEmbeddingFields::default(),
            model: None,
            dimensions: None,
            device: None,
        }
    }
}

impl EmbedSectionConfig {
    /// Compare declarations only; never resolve a provider to infer dimensions.
    pub fn compatibility_reasons(&self, default: Option<&EngineConfig>) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if self
            .model
            .as_deref()
            .is_some_and(|model| default.map(|engine| engine.name.as_str()) != Some(model))
        {
            reasons.push("embed.model differs from the default engine name");
        }
        if let (Some(dimensions), Some(declared)) =
            (self.dimensions, default.and_then(|engine| engine.dims))
        {
            if u64::try_from(declared).ok() != Some(dimensions) {
                reasons.push("embed.dimensions differs from the declared engine dimensions");
            }
        }
        if self.device.is_some() {
            reasons.push("embed.device is ignored");
        }
        reasons
    }
}

/// Ordered, exact entity field selectors; non-special names are literal keys.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct EntityEmbeddingFields {
    #[serde(default = "default_fields", deserialize_with = "included_fields")]
    include: Vec<String>,
}

impl Default for EntityEmbeddingFields {
    fn default() -> Self {
        Self {
            include: default_fields(),
        }
    }
}

impl EntityEmbeddingFields {
    pub fn include(&self) -> &[String] {
        &self.include
    }
}

fn default_fields() -> Vec<String> {
    vec!["name".to_owned(), "description".to_owned()]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyEmbedDevice {
    Metal,
    Cuda,
    Cpu,
}

impl<'de> Deserialize<'de> for LegacyEmbedDevice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "metal" => Ok(Self::Metal),
            "cuda" => Ok(Self::Cuda),
            "cpu" => Ok(Self::Cpu),
            _ => Err(serde::de::Error::custom("must be metal, cuda or cpu")),
        }
    }
}

fn positive_integer<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("must be a positive integer"));
    }
    Ok(Some(value))
}

fn legacy_model<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() {
        return Err(serde::de::Error::custom("must be a nonempty string"));
    }
    Ok(Some(value))
}

fn included_fields<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let fields = Vec::<String>::deserialize(deserializer)?;
    if fields.is_empty()
        || fields
            .iter()
            .any(|field| field.trim().is_empty() || field == "kind")
        || fields
            .iter()
            .enumerate()
            .any(|(index, field)| fields[..index].contains(field))
    {
        return Err(serde::de::Error::custom(
            "must contain nonblank, distinct field names other than kind",
        ));
    }
    Ok(fields)
}

// Derived structs accept sequences in some formats. Both declaration levels
// require maps, including when callers deserialize KhiveConfig directly.
fn table<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    struct TableVisitor<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for TableVisitor<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an embedding configuration table")
        }

        fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            T::deserialize(serde::de::value::MapAccessDeserializer::new(TableMap {
                map,
                first: true,
            }))
        }
    }
    deserializer.deserialize_map(TableVisitor(PhantomData))
}

struct TableMap<A> {
    map: A,
    first: bool,
}

impl<'de, A: serde::de::MapAccess<'de>> serde::de::MapAccess<'de> for TableMap<A> {
    type Error = A::Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: serde::de::DeserializeSeed<'de>,
    {
        let Some(key) = self.map.next_key::<String>()? else {
            return Ok(None);
        };
        // TOML transports datetimes as a map with this reserved key. Its own
        // Value deserializer recognizes the same marker; it is not a table.
        if self.first && key == "$__toml_private_datetime" {
            return Err(serde::de::Error::custom(
                "expected an embedding configuration table, got datetime",
            ));
        }
        self.first = false;
        seed.deserialize(key.into_deserializer()).map(Some)
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::DeserializeSeed<'de>,
    {
        self.map.next_value_seed(seed)
    }
}

pub(super) fn deserialize_table<'de, D>(deserializer: D) -> Result<EmbedSectionConfig, D::Error>
where
    D: serde::Deserializer<'de>,
{
    table(deserializer)
}

fn deserialize_fields<'de, D>(deserializer: D) -> Result<EntityEmbeddingFields, D::Error>
where
    D: serde::Deserializer<'de>,
{
    table(deserializer)
}

#[derive(Deserialize)]
struct SyntaxProbe {
    #[serde(default, deserialize_with = "deserialize_table")]
    embed: EmbedSectionConfig,
}

fn diagnostic(
    raw: &str,
    document: &toml::Value,
    path: &Path,
    original: toml::de::Error,
) -> ConfigError {
    let error = |key, source| ConfigError::SchemaParse {
        path: path.to_path_buf(),
        key,
        source: Box::new(source),
    };
    if document.get("embed").is_some_and(|value| !value.is_table()) {
        return error("embed", original);
    }
    // Only invalid input needs extra probes. Each probe reads original bytes,
    // preserving its own failing value span instead of guessing a key from text.
    macro_rules! probe {
        ($field:literal, $key:literal, $ty:ty, $decode:literal) => {{
            #[derive(Default, Deserialize)]
            struct Section {
                #[serde(rename = $field, default, deserialize_with = $decode)]
                _value: $ty,
            }
            #[derive(Deserialize)]
            struct Probe {
                #[serde(rename = "embed", default)]
                _embed: Section,
            }
            if let Err(source) = toml::from_str::<Probe>(raw) {
                return error($key, source);
            }
        }};
    }
    probe!("auto_embed", "embed.auto_embed", bool, "bool::deserialize");
    probe!(
        "batch_size",
        "embed.batch_size",
        Option<u64>,
        "positive_integer"
    );
    probe!("model", "embed.model", Option<String>, "legacy_model");
    probe!(
        "dimensions",
        "embed.dimensions",
        Option<u64>,
        "positive_integer"
    );
    probe!(
        "device",
        "embed.device",
        Option<LegacyEmbedDevice>,
        "Option::<LegacyEmbedDevice>::deserialize"
    );
    let fields_key = if document
        .get("embed")
        .and_then(|embed| embed.get("fields"))
        .is_some_and(|fields| !fields.is_table())
    {
        "embed.fields"
    } else {
        "embed.fields.include"
    };
    #[derive(Default, Deserialize)]
    struct FieldsSection {
        #[serde(rename = "fields", default, deserialize_with = "deserialize_fields")]
        _fields: EntityEmbeddingFields,
    }
    #[derive(Deserialize)]
    struct FieldsProbe {
        #[serde(rename = "embed", default)]
        _embed: FieldsSection,
    }
    match toml::from_str::<FieldsProbe>(raw) {
        Err(source) => error(fields_key, source),
        Ok(_) => error("embed", original),
    }
}

pub(super) fn parse(
    raw: &str,
    document: &toml::Value,
    path: &Path,
) -> Result<(EmbedSectionConfig, Vec<String>), ConfigError> {
    let probe: SyntaxProbe =
        toml::from_str(raw).map_err(|source| diagnostic(raw, document, path, source))?;
    let mut unknown = Vec::new();
    if let Some(table) = document.get("embed").and_then(toml::Value::as_table) {
        for key in table.keys() {
            if !matches!(
                key.as_str(),
                "batch_size" | "auto_embed" | "fields" | "model" | "dimensions" | "device"
            ) {
                unknown.push(key.clone());
            }
        }
        if let Some(fields) = table.get("fields").and_then(toml::Value::as_table) {
            unknown.extend(
                fields
                    .keys()
                    .filter(|key| key.as_str() != "include")
                    .map(|key| format!("fields.{key}")),
            );
        }
    }
    Ok((probe.embed, unknown))
}

#[cfg(test)]
#[path = "engine_config_embed_tests.rs"]
mod tests;
