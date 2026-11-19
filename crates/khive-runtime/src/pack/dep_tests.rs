use super::*;
use async_trait::async_trait;
use khive_types::Pack;
use serde_json::Value;

struct KgDepPack;
struct MemoryDepPack;
struct ADepPack;
struct BDepPack;

impl Pack for KgDepPack {
    const NAME: &'static str = "kg_dep";
    const NOTE_KINDS: &'static [&'static str] = &["observation"];
    const ENTITY_KINDS: &'static [&'static str] = &["concept"];
    const HANDLERS: &'static [HandlerDef] = &[];
}

impl Pack for MemoryDepPack {
    const NAME: &'static str = "memory_dep";
    const NOTE_KINDS: &'static [&'static str] = &["memory"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const REQUIRES: &'static [&'static str] = &["kg_dep"];
}

impl Pack for ADepPack {
    const NAME: &'static str = "pack_a";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const REQUIRES: &'static [&'static str] = &["pack_b"];
}

impl Pack for BDepPack {
    const NAME: &'static str = "pack_b";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const REQUIRES: &'static [&'static str] = &["pack_a"];
}

#[async_trait]
impl PackRuntime for KgDepPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "KgDepPack has no verbs: {verb}"
        )))
    }
}

#[async_trait]
impl PackRuntime for MemoryDepPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    fn requires(&self) -> &'static [&'static str] {
        Self::REQUIRES
    }
    async fn dispatch(
        &self,
        verb: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "MemoryDepPack has no verbs: {verb}"
        )))
    }
}

#[async_trait]
impl PackRuntime for ADepPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    fn requires(&self) -> &'static [&'static str] {
        Self::REQUIRES
    }
    async fn dispatch(
        &self,
        verb: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "ADepPack has no verbs: {verb}"
        )))
    }
}

#[async_trait]
impl PackRuntime for BDepPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    fn requires(&self) -> &'static [&'static str] {
        Self::REQUIRES
    }
    async fn dispatch(
        &self,
        verb: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "BDepPack has no verbs: {verb}"
        )))
    }
}

#[test]
fn test_pack_deps_happy_path() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(MemoryDepPack);
    builder.register(KgDepPack);
    let reg = builder
        .build()
        .expect("kg_dep satisfies memory_dep dependency");
    assert_eq!(reg.pack_requires("memory_dep").unwrap(), &["kg_dep"]);
    let names = reg.pack_names();
    let kg_pos = names.iter().position(|&n| n == "kg_dep").unwrap();
    let mem_pos = names.iter().position(|&n| n == "memory_dep").unwrap();
    assert!(
        kg_pos < mem_pos,
        "kg_dep must be loaded before memory_dep; order: {names:?}"
    );
}

#[test]
fn test_pack_deps_missing() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(MemoryDepPack);
    let err = match builder.build() {
        Ok(_) => panic!("expected Err, got Ok"),
        Err(e) => e,
    };
    assert!(
        matches!(err, RuntimeError::MissingPackDependency(_)),
        "expected MissingPackDependency, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("memory_dep"),
        "error must name the dependent pack: {msg}"
    );
    assert!(
        msg.contains("kg_dep"),
        "error must name the missing dep: {msg}"
    );
}

#[test]
fn test_pack_deps_circular() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(ADepPack);
    builder.register(BDepPack);
    let err = match builder.build() {
        Ok(_) => panic!("expected Err, got Ok"),
        Err(e) => e,
    };
    assert!(
        matches!(err, RuntimeError::CircularPackDependency(_)),
        "expected CircularPackDependency, got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("pack_a"), "error must name pack_a: {msg}");
    assert!(msg.contains("pack_b"), "error must name pack_b: {msg}");
}

#[test]
fn test_pack_deps_no_deps() {
    struct NoDepsA;
    struct NoDepsB;

    impl Pack for NoDepsA {
        const NAME: &'static str = "no_deps_a";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[];
    }

    impl Pack for NoDepsB {
        const NAME: &'static str = "no_deps_b";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[];
    }

    #[async_trait]
    impl PackRuntime for NoDepsA {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _: Value,
            _: &VerbRegistry,
            _: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Err(RuntimeError::InvalidInput(format!("NoDepsA: {verb}")))
        }
    }

    #[async_trait]
    impl PackRuntime for NoDepsB {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _: Value,
            _: &VerbRegistry,
            _: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Err(RuntimeError::InvalidInput(format!("NoDepsB: {verb}")))
        }
    }

    let mut builder = VerbRegistryBuilder::new();
    builder.register(NoDepsA);
    builder.register(NoDepsB);
    let reg = builder.build().expect("packs with REQUIRES=&[] build");
    assert_eq!(reg.pack_requires("no_deps_a").unwrap(), &[] as &[&str]);
    assert_eq!(reg.pack_requires("no_deps_b").unwrap(), &[] as &[&str]);
}

#[test]
fn test_pack_deps_all_missing_pairs_both_build_paths() {
    for metadata in [false, true] {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(MemoryDepPack);
        builder.register(ADepPack);
        let result = if metadata {
            builder.build_metadata().map(|_| ())
        } else {
            builder.build().map(|_| ())
        };
        let error =
            result.expect_err("both missing dependency pairs must refuse either build path");
        let RuntimeError::MissingPackDependencies(aggregate) = &error else {
            panic!("expected aggregate missing dependencies, metadata={metadata}, got {error:?}");
        };
        let pairs: Vec<_> = aggregate
            .missing
            .iter()
            .map(|missing| (missing.from.as_str(), missing.requires.as_str()))
            .collect();
        assert_eq!(
            pairs.as_slice(),
            &[("memory_dep", "kg_dep"), ("pack_a", "pack_b")],
            "metadata={metadata}"
        );
        assert_eq!(
            error.to_string(),
            concat!(
                "missing pack dependencies: ",
                "pack 'memory_dep' requires 'kg_dep', but 'kg_dep' is not in the loaded pack set; ",
                "pack 'pack_a' requires 'pack_b', but 'pack_b' is not in the loaded pack set"
            ),
            "metadata={metadata}"
        );
    }
}
