//! Namespace — string-based scoping for substrate records.
//!
//! In khive OSS, namespace is a plain string (e.g., `"local"`, `"research"`,
//! `"lattice-project"`). It groups records and supports cross-namespace
//! queries via the entity graph.
//!
//! Multi-tenant deployments (e.g., khive.ai hosted) add capability-based
//! access controls on top in a separate crate — those are not part of the
//! open-source runtime.

extern crate alloc;
use alloc::string::String;
use core::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Namespace(String);

impl Namespace {
    /// Create a namespace from any string-like value.
    #[inline]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The default namespace name.
    pub const DEFAULT: &'static str = "local";

    /// Construct the default namespace.
    pub fn default_ns() -> Self {
        Self::new(Self::DEFAULT)
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True if `self` is a hierarchical child of `parent`
    /// (e.g., `"research:lattice"` is a child of `"research"`).
    pub fn is_child_of(&self, parent: &Namespace) -> bool {
        self.0.len() > parent.0.len()
            && self.0.starts_with(parent.as_str())
            && self.0.as_bytes().get(parent.0.len()) == Some(&b':')
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl Default for Namespace {
    fn default() -> Self {
        Self::default_ns()
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Namespace {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Namespace {
    #[inline]
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for Namespace {
    #[inline]
    fn from(s: String) -> Self {
        Self(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction() {
        let ns = Namespace::new("research");
        assert_eq!(ns.as_str(), "research");
    }

    #[test]
    fn default_is_local() {
        assert_eq!(Namespace::default().as_str(), "local");
    }

    #[test]
    fn is_child_of() {
        let parent = Namespace::new("research");
        let child = Namespace::new("research:lattice");
        let sibling = Namespace::new("other");

        assert!(child.is_child_of(&parent));
        assert!(!sibling.is_child_of(&parent));
        assert!(!parent.is_child_of(&parent));
    }
}
