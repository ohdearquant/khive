use std::ops::Range;

use khive_storage::secret_scan::{PropertyScanContext, SecretMatch};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{RuntimeError, RuntimeResult};

const DOMAIN: &[u8] = b"khive-secret-gate-v1\0json-properties-context-v2\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtomRole {
    Key,
    Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathSegment {
    Key(String),
    Index(usize),
}

#[derive(Debug)]
pub(crate) struct PropertyAtom {
    value: String,
    role: AtomRole,
    path: Vec<PathSegment>,
    span: Range<usize>,
    digest: [u8; 32],
}

impl PropertyAtom {
    pub(crate) fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    #[cfg(test)]
    pub(crate) fn identifies(&self, role: AtomRole, path: &[PathSegment]) -> bool {
        self.role == role && self.path == path
    }
}

pub(crate) struct PropertyContext {
    canonical: String,
    atoms: Vec<PropertyAtom>,
}

pub(crate) struct ScannedAtom<'a> {
    pub(crate) atom: &'a PropertyAtom,
    pub(crate) matches: Vec<SecretMatch>,
}

fn framing_error() -> RuntimeError {
    RuntimeError::InvalidInput("secret-gate property context framing failed".into())
}

fn length(value: usize) -> RuntimeResult<[u8; 8]> {
    Ok(u64::try_from(value)
        .map_err(|_| framing_error())?
        .to_be_bytes())
}

impl PropertyContext {
    pub(crate) fn new(value: &Value) -> RuntimeResult<Self> {
        let mut context = Self {
            canonical: String::new(),
            atoms: Vec::new(),
        };
        context.write_value(value, &mut Vec::new())?;
        let mut context_hash = Sha256::new();
        context_hash.update(DOMAIN);
        context_hash.update([1]);
        context_hash.update(length(context.canonical.len())?);
        context_hash.update(context.canonical.as_bytes());
        let context_digest: [u8; 32] = context_hash.finalize().into();
        for atom in &mut context.atoms {
            let mut hash = Sha256::new();
            hash.update(DOMAIN);
            hash.update([2]);
            hash.update(context_digest);
            hash.update([match atom.role {
                AtomRole::Key => 0,
                AtomRole::Value => 1,
            }]);
            hash.update(length(atom.path.len())?);
            for segment in &atom.path {
                match segment {
                    PathSegment::Key(key) => {
                        hash.update([0]);
                        hash.update(length(key.len())?);
                        hash.update(key.as_bytes());
                    }
                    PathSegment::Index(index) => {
                        hash.update([1]);
                        hash.update(length(*index)?);
                    }
                }
            }
            hash.update(length(atom.value.len())?);
            hash.update(atom.value.as_bytes());
            atom.digest = hash.finalize().into();
        }
        Ok(context)
    }

    fn write_string(
        &mut self,
        value: &str,
        role: AtomRole,
        path: &[PathSegment],
    ) -> RuntimeResult<()> {
        let start = self.canonical.len();
        self.canonical
            .push_str(&serde_json::to_string(value).map_err(|_| framing_error())?);
        self.atoms.push(PropertyAtom {
            value: value.into(),
            role,
            path: path.to_vec(),
            span: start..self.canonical.len(),
            digest: [0; 32],
        });
        Ok(())
    }

    fn write_value(&mut self, value: &Value, path: &mut Vec<PathSegment>) -> RuntimeResult<()> {
        match value {
            Value::String(value) => self.write_string(value, AtomRole::Value, path)?,
            Value::Object(object) => {
                self.canonical.push('{');
                let mut members: Vec<_> = object.iter().collect();
                members.sort_unstable_by(|(left, _), (right, _)| {
                    left.as_bytes().cmp(right.as_bytes())
                });
                for (index, (key, value)) in members.into_iter().enumerate() {
                    if index != 0 {
                        self.canonical.push(',');
                    }
                    path.push(PathSegment::Key(key.clone()));
                    self.write_string(key, AtomRole::Key, path)?;
                    self.canonical.push(':');
                    self.write_value(value, path)?;
                    path.pop();
                }
                self.canonical.push('}');
            }
            Value::Array(array) => {
                self.canonical.push('[');
                for (index, value) in array.iter().enumerate() {
                    if index != 0 {
                        self.canonical.push(',');
                    }
                    path.push(PathSegment::Index(index));
                    self.write_value(value, path)?;
                    path.pop();
                }
                self.canonical.push(']');
            }
            _ => self
                .canonical
                .push_str(&serde_json::to_string(value).map_err(|_| framing_error())?),
        }
        Ok(())
    }

    pub(crate) fn scan_atoms(
        &self,
    ) -> RuntimeResult<impl Iterator<Item = RuntimeResult<ScannedAtom<'_>>> + '_> {
        let scanner = PropertyScanContext::new(&self.canonical).map_err(|_| {
            RuntimeError::InvalidInput("secret-gate property origin mapping failed".into())
        })?;
        Ok(self.atoms.iter().map(move |atom| {
            let matches = scanner.scan_atom(atom.span.clone()).map_err(|_| {
                RuntimeError::InvalidInput("secret-gate property origin mapping failed".into())
            })?;
            Ok(ScannedAtom { atom, matches })
        }))
    }

    #[cfg(test)]
    pub(crate) fn atom(&self, role: AtomRole, path: &[PathSegment]) -> Option<&PropertyAtom> {
        self.atoms.iter().find(|atom| atom.identifies(role, path))
    }
}

pub(crate) fn check(value: &Value) -> RuntimeResult<()> {
    let context = PropertyContext::new(value)?;
    for scanned in context.scan_atoms()? {
        let scanned = scanned?;
        if let Some(matched) = scanned.matches.into_iter().next() {
            return Err(RuntimeError::SecretDetected(matched));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "property_context_tests.rs"]
mod tests;
