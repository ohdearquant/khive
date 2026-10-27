use super::*;
use async_trait::async_trait;
use khive_types::Pack;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

// ── HelpPack: a minimal pack with one handler that records invocation count.
//
// Used to verify that help=true never reaches the pack's dispatch method.

static CREATE_PARAMS: [ParamDef; 2] = [
    ParamDef {
        name: "kind",
        param_type: "string",
        required: true,
        description: "Granular kind (concept | document | ...).",
        resolution_mode: IdResolutionMode::NotApplicable,
    },
    ParamDef {
        name: "name",
        param_type: "string",
        required: false,
        description: "Human-readable name.",
        resolution_mode: IdResolutionMode::NotApplicable,
    },
];

static RECALL_PARAMS: [ParamDef; 2] = [
    ParamDef {
        name: "query",
        param_type: "string",
        required: true,
        description: "Semantic recall query.",
        resolution_mode: IdResolutionMode::NotApplicable,
    },
    ParamDef {
        name: "limit",
        param_type: "integer",
        required: false,
        description: "Maximum memories to return.",
        resolution_mode: IdResolutionMode::NotApplicable,
    },
];

// A subhandler with no params — mirrors recall.embed / brain.emit / etc.
// Used to test that help=true on a Subhandler returns callable_via_mcp: false.
static EMBED_PARAMS: [ParamDef; 0] = [];

// A uuid-typed param declaring IdResolutionMode::UnscopedById, used to
// verify `describe_verb` appends `resolution_mode_contract`'s rendering
// to every uuid-typed description instead of requiring each `HandlerDef`
// to paste the contract in by hand.
static GET_PARAMS: [ParamDef; 1] = [ParamDef {
    name: "id",
    param_type: "uuid",
    required: true,
    description: "UUID of the record to fetch.",
    resolution_mode: IdResolutionMode::UnscopedById,
}];

// Mirrors link's real source_id/target_id params (both `param_type:
// "uuid"`, `IdResolutionMode::UnscopedById`) — used to verify the shared
// id contract is appended to link's endpoint params too, matching the
// enumeration in `resolution_mode_contract`'s `UnscopedById` text.
static LINK_PARAMS: [ParamDef; 2] = [
    ParamDef {
        name: "source_id",
        param_type: "uuid",
        required: true,
        description: "Source node UUID.",
        resolution_mode: IdResolutionMode::UnscopedById,
    },
    ParamDef {
        name: "target_id",
        param_type: "uuid",
        required: true,
        description: "Target node UUID.",
        resolution_mode: IdResolutionMode::UnscopedById,
    },
];

struct HelpPack {
    invocations: Arc<AtomicUsize>,
}

impl Pack for HelpPack {
    const NAME: &'static str = "helptest";
    const NOTE_KINDS: &'static [&'static str] = &["task"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[
        HandlerDef {
            name: "create",
            description: "Create an entity or note",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &CREATE_PARAMS,
        },
        HandlerDef {
            name: "recall",
            description: "Recall memory notes with decay-aware hybrid ranking",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &RECALL_PARAMS,
        },
        // A Subhandler used to test that help=true returns
        // callable_via_mcp: false for internal verbs.
        HandlerDef {
            name: "recall.embed",
            description: "Return the embedding vector used by memory recall",
            visibility: Visibility::Subhandler,
            category: VerbCategory::Assertive,
            params: &EMBED_PARAMS,
        },
        HandlerDef {
            name: "link",
            description: "Create a typed directed edge",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &LINK_PARAMS,
        },
        HandlerDef {
            name: "get",
            description: "Fetch a record by id",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &GET_PARAMS,
        },
    ];
}

// A pack-declared additive edge rule (mirrors the GTD pack's real
// task-to-task `depends_on` rule), used to verify `link(help=true)`
// surfaces pack-composed rules alongside the base entity table. The
// second entry declares a rule for a special relation
// (`supersedes`) that the validator's dedicated special-relation
// branch never consults `pack_rule_allows` for — it must NOT be
// advertised (see `test_link_help_true_matches_special_relation_validator_set`).
static HELP_EDGE_RULES: [EdgeEndpointRule; 2] = [
    EdgeEndpointRule {
        relation: khive_types::EdgeRelation::DependsOn,
        source: EndpointKind::NoteOfKind("task"),
        target: EndpointKind::NoteOfKind("task"),
    },
    EdgeEndpointRule {
        relation: khive_types::EdgeRelation::Supersedes,
        source: EndpointKind::NoteOfKind("task"),
        target: EndpointKind::NoteOfKind("task"),
    },
];

#[async_trait]
impl PackRuntime for HelpPack {
    fn name(&self) -> &str {
        HelpPack::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        HelpPack::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        HelpPack::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        HelpPack::HANDLERS
    }
    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        &HELP_EDGE_RULES
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({ "pack": "helptest", "verb": verb }))
    }
}

fn build_help_registry(invocations: Arc<AtomicUsize>) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(HelpPack { invocations });
    builder.build().expect("help registry builds")
}

include!("help_tests/schema_and_identifier_description_tests.rs");

include!("help_tests/recall_and_link_description_tests.rs");

/// help=true is intercepted before pack dispatch — the pack's dispatch method
/// must never be invoked when help=true is in the params.
#[tokio::test]
async fn test_help_true_does_not_execute_the_verb() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let reg = build_help_registry(invocations.clone());

    // Call both verbs with help=true.
    reg.dispatch("create", serde_json::json!({ "help": true }))
        .await
        .expect("help=true must succeed");
    reg.dispatch("recall", serde_json::json!({ "help": true }))
        .await
        .expect("help=true must succeed");

    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "pack dispatch MUST NOT be invoked when help=true; \
             got {} invocation(s)",
        invocations.load(Ordering::SeqCst)
    );

    // Confirm that a normal call (without help=true) DOES invoke dispatch.
    reg.dispatch("create", serde_json::json!({}))
        .await
        .expect("normal dispatch must succeed");
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "pack dispatch must fire exactly once for a normal call"
    );
}

// ── Subhandler help-schema regressions ─────────────────────────────────
//
// Subhandler verbs must return `callable_via_mcp: false` in their help
// schema so agents who read help=true before probing see accurate
// availability — not a "looks callable" schema followed by permission denied.

/// help=true on a `Visibility::Subhandler` verb returns `callable_via_mcp: false`
/// and `visibility: "internal"` rather than a plain callable-looking envelope.
#[tokio::test]
async fn help_true_on_subhandler_returns_callable_via_mcp_false() {
    let reg = build_help_registry(Arc::new(AtomicUsize::new(0)));

    let result = reg
        .dispatch("recall.embed", serde_json::json!({ "help": true }))
        .await
        .expect("help=true on subhandler must succeed (no permission check on help path)");

    assert_eq!(
        result["callable_via_mcp"],
        serde_json::json!(false),
        "subhandler help must carry callable_via_mcp: false"
    );
    assert_eq!(
        result["visibility"], "internal",
        "subhandler help must carry visibility: internal"
    );
    // The verb and pack fields must still be present so the caller knows
    // what the schema belongs to.
    assert_eq!(result["verb"], "recall.embed");
    assert_eq!(result["pack"], "helptest");
}

/// Public Verb-visibility handlers must NOT have `callable_via_mcp: false`.
#[tokio::test]
async fn help_true_on_public_verb_does_not_have_callable_via_mcp_false() {
    let reg = build_help_registry(Arc::new(AtomicUsize::new(0)));

    let result = reg
        .dispatch("create", serde_json::json!({ "help": true }))
        .await
        .expect("help=true on public verb must succeed");

    // callable_via_mcp must be absent or true for public verbs.
    assert_ne!(
        result.get("callable_via_mcp"),
        Some(&serde_json::json!(false)),
        "public verb help must NOT carry callable_via_mcp: false"
    );
    // visibility must be absent or 'public' (never 'internal') for public verbs.
    assert_ne!(
        result.get("visibility"),
        Some(&serde_json::json!("internal")),
        "public verb help must NOT carry visibility: internal"
    );
}

/// help=true on an unknown verb returns an error (same behavior as normal dispatch).
#[tokio::test]
async fn help_true_on_unknown_verb_returns_error() {
    let reg = build_help_registry(Arc::new(AtomicUsize::new(0)));

    let err = reg
        .dispatch("nonexistent_verb", serde_json::json!({ "help": true }))
        .await
        .unwrap_err();

    assert!(
        matches!(err, RuntimeError::UnknownVerb(_)),
        "help=true on unknown verb must return UnknownVerb, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("nonexistent_verb"),
        "error must name the unknown verb: {msg}"
    );
}

/// Subhandler help must include params: [] even when the verb has no params.
#[tokio::test]
async fn help_true_on_subhandler_includes_params_field() {
    let reg = build_help_registry(Arc::new(AtomicUsize::new(0)));

    let result = reg
        .dispatch("recall.embed", serde_json::json!({ "help": true }))
        .await
        .expect("help=true on subhandler must succeed");

    // params must always be present (consistent shape).
    let params = result
        .get("params")
        .expect("subhandler help must include 'params' field");
    assert!(
        params.is_array(),
        "subhandler help params must be a JSON array"
    );
}

// ── Unknown-verb error must not leak subhandler names ─────────

/// `describe_verb` on an unknown verb must list only Verb-visibility names
/// in the "available" list: never subhandler names like `recall.embed`.
#[tokio::test]
async fn help_true_unknown_verb_available_list_excludes_subhandlers() {
    let reg = build_help_registry(Arc::new(AtomicUsize::new(0)));

    let err = reg
        .dispatch("not_a_verb", serde_json::json!({ "help": true }))
        .await
        .unwrap_err();

    let msg = err.to_string();
    // `recall.embed` is a Subhandler in HelpPack — must NOT appear in the
    // "available" list of an unknown-verb error.
    assert!(
        !msg.contains("recall.embed"),
        "unknown-verb help error must not advertise subhandler recall.embed: {msg}"
    );
    // Public verbs must still appear so the agent knows what to call.
    assert!(
        msg.contains("create"),
        "unknown-verb help error must still list public verb 'create': {msg}"
    );
    assert!(
        msg.contains("recall"),
        "unknown-verb help error must still list public verb 'recall': {msg}"
    );
}

/// Normal dispatch on an unknown verb must also not leak subhandler names.
#[tokio::test]
async fn dispatch_unknown_verb_available_list_excludes_subhandlers() {
    let reg = build_help_registry(Arc::new(AtomicUsize::new(0)));

    let err = reg
        .dispatch("not_a_verb", serde_json::json!({}))
        .await
        .unwrap_err();

    let msg = err.to_string();
    // `recall.embed` is a Subhandler in HelpPack — must NOT appear in the
    // "available" list of an unknown-verb dispatch error.
    assert!(
        !msg.contains("recall.embed"),
        "dispatch unknown-verb error must not advertise subhandler recall.embed: {msg}"
    );
    // Public verbs must still appear so the agent knows what to call.
    assert!(
        msg.contains("create"),
        "dispatch unknown-verb error must still list public verb 'create': {msg}"
    );
    assert!(
        msg.contains("recall"),
        "dispatch unknown-verb error must still list public verb 'recall': {msg}"
    );
}

// ── ADR-028 multi-backend schema routing tests ───────────────────────────

/// A test pack that returns a real SchemaPlan so we can assert routing.
struct SchemaPack {
    pack_name: &'static str,
    statements: &'static [&'static str],
    column_additions: &'static [PackColumnAddition],
}

impl Pack for SchemaPack {
    const NAME: &'static str = "schema-pack";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}

#[async_trait]
impl PackRuntime for SchemaPack {
    fn name(&self) -> &str {
        self.pack_name
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        &[]
    }
    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: self.pack_name,
            statements: self.statements,
        }
    }
    fn schema_column_additions(&self) -> &'static [PackColumnAddition] {
        self.column_additions
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(serde_json::json!({ "pack": self.pack_name, "verb": verb }))
    }
}

// ADR-028: all_schema_plans_named returns (pack_name, SchemaPlan) pairs
// where pack_name comes from SchemaPlan::pack (always &'static str).
#[test]
fn all_schema_plans_named_returns_correct_pairs() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "alpha",
        statements: &["CREATE TABLE IF NOT EXISTS t_alpha (id INTEGER PRIMARY KEY)"],
        column_additions: &[],
    }));
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "beta",
        statements: &[],
        column_additions: &[],
    }));
    let reg = builder.build().expect("registry builds");

    let named = reg.all_schema_plans_named();
    assert_eq!(named.len(), 2);

    let alpha_entry = named.iter().find(|(n, _)| *n == "alpha");
    let beta_entry = named.iter().find(|(n, _)| *n == "beta");

    assert!(alpha_entry.is_some(), "alpha must appear in named plans");
    assert!(beta_entry.is_some(), "beta must appear in named plans");

    let (_, alpha_plan) = alpha_entry.unwrap();
    assert_eq!(alpha_plan.statements.len(), 1);
    assert!(!alpha_plan.is_empty());

    let (_, beta_plan) = beta_entry.unwrap();
    assert!(beta_plan.is_empty());
}

// ADR-028: apply_schema_plans_with_map routes non-empty plans to the
// correct per-pack backend instead of the default.
//
// Verification: apply DDL to routed backend, then confirm the table is
// present on pack_backend and absent on default_backend by attempting to
// apply the same DDL again — if the table already exists on pack_backend
// the idempotent CREATE IF NOT EXISTS succeeds; applying to default_backend
// would only matter if the table were routed there.  We verify isolation
// by applying the plan and then running a targeted DDL on each backend
// that would fail if the table did not already exist (CREATE without
// IF NOT EXISTS on a duplicate raises an error), combined with a no-error
// path on the correct backend.
//
// Simpler approach: confirm the plan applies without error (routing is
// correct) and that the opposite backend returns an error when we try to
// INSERT into the routed table (table-not-found = SQLITE_ERROR).
#[tokio::test]
async fn apply_schema_plans_with_map_routes_to_correct_backend() {
    use khive_storage::types::{SqlStatement, SqlValue};

    let default_backend = khive_db::StorageBackend::memory().expect("default memory backend");
    let pack_backend = khive_db::StorageBackend::memory().expect("pack-specific memory backend");

    let mut builder = VerbRegistryBuilder::new();
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "routed",
        statements: &["CREATE TABLE IF NOT EXISTS t_routed (id INTEGER PRIMARY KEY)"],
        column_additions: &[],
    }));
    let reg = builder.build().expect("registry builds");

    let mut backend_map: HashMap<&str, &khive_db::StorageBackend> = HashMap::new();
    backend_map.insert("routed", &pack_backend);

    reg.apply_schema_plans_with_map(&backend_map, &default_backend)
        .expect("schema application must not collide");

    // On pack_backend: INSERT must succeed (table exists).
    let mut writer = pack_backend.sql().writer().await.expect("writer");
    let result = writer
        .execute(SqlStatement {
            sql: "INSERT INTO t_routed (id) VALUES (?1)".into(),
            params: vec![SqlValue::Integer(1)],
            label: None,
        })
        .await;
    assert!(
        result.is_ok(),
        "t_routed must exist on pack_backend after routing: {result:?}"
    );

    // On default_backend: INSERT must fail (table not there).
    let mut default_writer = default_backend.sql().writer().await.expect("writer");
    let default_result = default_writer
        .execute(SqlStatement {
            sql: "INSERT INTO t_routed (id) VALUES (?1)".into(),
            params: vec![SqlValue::Integer(2)],
            label: None,
        })
        .await;
    assert!(
        default_result.is_err(),
        "t_routed must NOT exist on default_backend (table should not be there)"
    );
}

// ADR-028: apply_schema_plans_with_map uses default backend for packs
// absent from the map.
#[tokio::test]
async fn apply_schema_plans_with_map_falls_back_to_default_for_unmapped_packs() {
    use khive_storage::types::{SqlStatement, SqlValue};

    let default_backend = khive_db::StorageBackend::memory().expect("default memory backend");

    let mut builder = VerbRegistryBuilder::new();
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "unmapped",
        statements: &["CREATE TABLE IF NOT EXISTS t_unmapped (id INTEGER PRIMARY KEY)"],
        column_additions: &[],
    }));
    let reg = builder.build().expect("registry builds");

    let backend_map: HashMap<&str, &khive_db::StorageBackend> = HashMap::new();
    reg.apply_schema_plans_with_map(&backend_map, &default_backend)
        .expect("schema application must not collide");

    // On default_backend: INSERT must succeed (table fell back here).
    let mut writer = default_backend.sql().writer().await.expect("writer");
    let result = writer
        .execute(SqlStatement {
            sql: "INSERT INTO t_unmapped (id) VALUES (?1)".into(),
            params: vec![SqlValue::Integer(1)],
            label: None,
        })
        .await;
    assert!(
        result.is_ok(),
        "t_unmapped must exist on default_backend for unmapped pack: {result:?}"
    );
}

// ADR-028: two packs declaring the same auxiliary table on the same
// backend must cause apply_schema_plans_with_map to return an error that
// names both packs and the table: it is a boot-time failure, not a
// silent DDL race.
#[test]
fn apply_schema_plans_with_map_collision_is_an_error() {
    let backend = khive_db::StorageBackend::memory().expect("memory backend");
    let empty_map: HashMap<&str, &khive_db::StorageBackend> = HashMap::new();

    let mut builder = VerbRegistryBuilder::new();
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "pack_alpha",
        statements: &["CREATE TABLE IF NOT EXISTS collision_table (id INTEGER PRIMARY KEY)"],
        column_additions: &[],
    }));
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "pack_beta",
        statements: &["CREATE TABLE IF NOT EXISTS collision_table (id INTEGER PRIMARY KEY)"],
        column_additions: &[],
    }));
    let registry = builder.build().expect("registry builds");

    let result = registry.apply_schema_plans_with_map(&empty_map, &backend);
    assert!(
        result.is_err(),
        "two packs declaring the same table on the same backend must produce a collision error"
    );
    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("pack_alpha"),
        "collision error must name first pack; got: {msg}"
    );
    assert!(
        msg.contains("pack_beta"),
        "collision error must name second pack; got: {msg}"
    );
    assert!(
        msg.contains("collision_table"),
        "collision error must name the table; got: {msg}"
    );
}

#[test]
fn schema_collision_checks_every_statement_before_any_ddl() {
    let cases: [(
        &'static str,
        &'static [&'static str],
        &'static [&'static str],
    ); 2] = [
        (
            "shared",
            &["CREATE TABLE IF NOT EXISTS shared (id INTEGER)"],
            &["CREATE TABLE IF NOT EXISTS first_table (id INTEGER); \
               CREATE TABLE IF NOT EXISTS shared (id INTEGER);"],
        ),
        (
            "shared;table",
            &[r#"CREATE TABLE IF NOT EXISTS "shared;table" (id INTEGER)"#],
            &[r#"
                ; CREATE TABLE IF NOT EXISTS first_table (id INTEGER);
                CREATE INDEX IF NOT EXISTS first_index ON first_table(id);
                /* a comment; between declarations */
                CREATE TABLE IF NOT EXISTS main."shared;table" (id INTEGER)
            "#],
        ),
    ];
    for (table, first_pack, second_pack) in cases {
        let backend = khive_db::StorageBackend::memory().expect("memory backend");
        let mut builder = VerbRegistryBuilder::new();
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "pack_alpha",
            statements: first_pack,
            column_additions: &[],
        }));
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "pack_beta",
            statements: second_pack,
            column_additions: &[],
        }));
        let registry = builder.build().expect("registry builds");
        let error = registry
            .apply_schema_plans_with_map(&HashMap::new(), &backend)
            .expect_err("later statements must not evade table ownership");
        assert_eq!(error.pack_a, "pack_alpha");
        assert_eq!(error.pack_b, "pack_beta");
        assert_eq!(error.table, table);
        let table_count: i64 = backend
            .pool()
            .reader()
            .expect("reader")
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name IN (?1, 'first_table')",
                [table],
                |row| row.get(0),
            )
            .expect("schema count");
        assert_eq!(table_count, 0, "collision must precede all pack DDL");
    }
}

#[test]
fn schema_multi_statement_entries_preserve_quoted_and_comment_semicolons() {
    let backend = khive_db::StorageBackend::memory().expect("memory backend");
    let mut builder = VerbRegistryBuilder::new();
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "pack_alpha",
        statements: &["CREATE TABLE IF NOT EXISTS shared (id INTEGER)"],
        column_additions: &[],
    }));
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "pack_beta",
        statements: &[r#"
            ;; CREATE TABLE IF NOT EXISTS "first;table" (
                id INTEGER,
                value TEXT DEFAULT 'it''s; CREATE TABLE shared (id INTEGER); still text'
            );
            -- ; CREATE TABLE shared (id INTEGER);
            CREATE INDEX IF NOT EXISTS first_index ON "first;table"(id);
            /* ; CREATE TABLE shared (id INTEGER); */
            CREATE TABLE IF NOT EXISTS unique_second (id INTEGER)
        "#],
        column_additions: &[],
    }));
    let registry = builder.build().expect("registry builds");
    registry
        .apply_schema_plans_with_map(&HashMap::new(), &backend)
        .expect("unique tables must register despite semicolons inside quotes or comments");

    let reader = backend.pool().reader().expect("reader");
    for table in ["shared", "first;table", "unique_second"] {
        let table_count: i64 = reader
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("schema count");
        assert_eq!(table_count, 1, "every table must be installed: {table}");
    }
    let default_value: String = reader
        .query_row(
            "SELECT dflt_value FROM pragma_table_info(?1) WHERE name = 'value'",
            ["first;table"],
            |row| row.get(0),
        )
        .expect("quoted default");
    assert_eq!(
        default_value,
        "'it''s; CREATE TABLE shared (id INTEGER); still text'"
    );
}

#[test]
fn schema_collision_normalizes_sql_identifiers_before_any_ddl() {
    let spellings: [&'static [&'static str]; 7] = [
        &["CREATE TABLE IF NOT EXISTS \"shared\"(id INTEGER)"],
        &["-- pack table\nCREATE TABLE IF NOT EXISTS shared(id INTEGER)"],
        &["CREATE TABLE IF NOT EXISTS main.shared(id INTEGER)"],
        &["CREATE TEMP TABLE IF NOT EXISTS shared(id INTEGER)"],
        &["CREATE/**/TABLE IF NOT EXISTS shared(id INTEGER)"],
        &["CREATE TABLE IF NOT EXISTS/**/shared(id INTEGER)"],
        &["CREATE TABLE IF NOT EXISTS shared/**/(id INTEGER)"],
    ];
    for statements in spellings {
        let backend = khive_db::StorageBackend::memory().expect("memory backend");
        let mut builder = VerbRegistryBuilder::new();
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "pack_alpha",
            statements: &["CREATE TABLE IF NOT EXISTS shared (id INTEGER)"],
            column_additions: &[],
        }));
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "pack_beta",
            statements,
            column_additions: &[],
        }));
        let registry = builder.build().expect("registry builds");
        let error = registry
            .apply_schema_plans_with_map(&HashMap::new(), &backend)
            .expect_err("spelling must not evade table ownership");
        let message = error.to_string();
        assert!(message.contains("pack_alpha") && message.contains("pack_beta"));
        assert!(message.contains("shared"));
        let table_count: i64 = backend
            .pool()
            .reader()
            .expect("reader")
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'shared'",
                [],
                |row| row.get(0),
            )
            .expect("schema count");
        assert_eq!(table_count, 0, "collision must precede all pack DDL");
    }
}

#[test]
fn sqlite_accepts_single_quoted_table_names() {
    let cases = [
        ("shared", "CREATE TABLE 'shared' (id INTEGER)"),
        ("sh'ared", "CREATE TABLE 'sh''ared' (id INTEGER)"),
    ];
    for (name, statement) in cases {
        let backend = khive_db::StorageBackend::memory().expect("memory backend");
        backend
            .apply_pack_ddl_statements(&[statement])
            .expect("SQLite accepts the table name");
        let actual: String = backend
            .pool()
            .reader()
            .expect("reader")
            .query_row(
                "SELECT name FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [name],
                |row| row.get(0),
            )
            .expect("created table");
        assert_eq!(actual, name);
    }
}

#[test]
fn single_quoted_table_names_collide_before_ddl() {
    let cases: [(
        &'static str,
        &'static [&'static str],
        &'static [&'static str],
    ); 2] = [
        (
            "shared",
            &["CREATE TABLE IF NOT EXISTS shared (id INTEGER)"],
            &["CREATE TABLE IF NOT EXISTS 'shared' (id INTEGER)"],
        ),
        (
            "sh'ared",
            &["CREATE TABLE IF NOT EXISTS \"sh'ared\" (id INTEGER)"],
            &["CREATE TABLE IF NOT EXISTS 'sh''ared' (id INTEGER)"],
        ),
    ];
    for (name, unquoted_or_double_quoted, single_quoted) in cases {
        let backend = khive_db::StorageBackend::memory().expect("memory backend");
        let mut builder = VerbRegistryBuilder::new();
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "pack_alpha",
            statements: unquoted_or_double_quoted,
            column_additions: &[],
        }));
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "pack_beta",
            statements: single_quoted,
            column_additions: &[],
        }));
        let registry = builder.build().expect("registry builds");
        let error = registry
            .apply_schema_plans_with_map(&HashMap::new(), &backend)
            .expect_err("both declarations own one SQLite table");
        assert_eq!(error.pack_a, "pack_alpha");
        assert_eq!(error.pack_b, "pack_beta");
        assert_eq!(error.table, name);
        let table_count: i64 = backend
            .pool()
            .reader()
            .expect("reader")
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [name],
                |row| row.get(0),
            )
            .expect("schema count");
        assert_eq!(table_count, 0, "collision must precede all pack DDL");
    }
}

#[test]
fn quoted_punctuation_table_names_cannot_evade_ownership() {
    let cases: [(&'static str, &'static [&'static str]); 3] = [
        (".", &[r#"CREATE TABLE IF NOT EXISTS "." (id INTEGER)"#]),
        ("(", &[r#"CREATE TABLE IF NOT EXISTS "(" (id INTEGER)"#]),
        (";", &[r#"CREATE TABLE IF NOT EXISTS ";" (id INTEGER)"#]),
    ];
    for (table, statements) in cases {
        // Control: the quoted-punctuation identifier is valid SQLite on its
        // own, so a refusal below must be an ownership-collision refusal,
        // not a generic SQL-syntax rejection.
        let control = khive_db::StorageBackend::memory().expect("control backend");
        control
            .apply_pack_ddl_statements(statements)
            .expect("quoted punctuation is valid SQLite");

        let backend = khive_db::StorageBackend::memory().expect("memory backend");
        let mut builder = VerbRegistryBuilder::new();
        for pack_name in ["pack_alpha", "pack_beta"] {
            builder.register_boxed(Box::new(SchemaPack {
                pack_name,
                statements,
                column_additions: &[],
            }));
        }
        let registry = builder.build().expect("registry builds");
        let error = registry
            .apply_schema_plans_with_map(&HashMap::new(), &backend)
            .expect_err("quoted punctuation must remain an owned table");
        let message = error.to_string();
        assert!(message.contains("pack_alpha") && message.contains("pack_beta"));
        let table_count: i64 = backend
            .pool()
            .reader()
            .expect("reader")
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("schema count");
        assert_eq!(table_count, 0, "collision must precede all pack DDL");
    }
}

#[test]
fn apply_schema_plans_with_map_read_only_collision_is_an_error_without_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("read_only_schema_collision.db");
    {
        let writable = khive_db::StorageBackend::sqlite_for_test(&path).expect("writable backend");
        writable.prepare_core_schema().expect("current schema");
    }
    #[cfg(unix)]
    khive_storage::test_support::freeze_snapshot_sidecars(&path);
    let backend =
        khive_db::StorageBackend::sqlite_read_only_for_test(&path).expect("read-only backend");
    let empty_map: HashMap<&str, &khive_db::StorageBackend> = HashMap::new();

    let mut builder = VerbRegistryBuilder::new();
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "pack_alpha",
        statements: &["CREATE TABLE IF NOT EXISTS collision_table (id INTEGER PRIMARY KEY)"],
        column_additions: &[],
    }));
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "pack_beta",
        statements: &["CREATE TABLE IF NOT EXISTS collision_table (id INTEGER PRIMARY KEY)"],
        column_additions: &[],
    }));
    let registry = builder.build().expect("registry builds");
    let writes_before = backend.pool().writer_acquisition_snapshot();

    let result = registry.apply_schema_plans_with_map(&empty_map, &backend);

    let err = result.expect_err(
        "read-only topology must reject the same cross-pack collision as writable topology",
    );
    let msg = err.to_string();
    assert!(
        msg.contains("pack_alpha"),
        "collision error must name first pack; got: {msg}"
    );
    assert!(
        msg.contains("pack_beta"),
        "collision error must name second pack; got: {msg}"
    );
    assert!(
        msg.contains("collision_table"),
        "collision error must name the table; got: {msg}"
    );
    assert_eq!(
        backend.pool().writer_acquisition_snapshot(),
        writes_before,
        "read-only collision validation must not acquire a writer"
    );
}

fn column_schema_registry() -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "alpha",
        statements: &["CREATE TABLE IF NOT EXISTS t_alpha (id INTEGER, revision TEXT)"],
        column_additions: &[PackColumnAddition {
            table: "t_alpha",
            column: "revision",
            affinity: PackColumnAffinity::Text,
        }],
    }));
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "beta",
        statements: &["CREATE TABLE IF NOT EXISTS t_beta (id INTEGER, epoch INTEGER)"],
        column_additions: &[PackColumnAddition {
            table: "t_beta",
            column: "epoch",
            affinity: PackColumnAffinity::Integer,
        }],
    }));
    builder.build().expect("registry builds")
}

fn seed_column_schema(backend: &khive_db::StorageBackend) {
    backend
        .apply_pack_ddl_statements(&[
            "CREATE TABLE t_alpha (id INTEGER)",
            "CREATE TABLE t_beta (id INTEGER)",
        ])
        .expect("legacy schemas");
}

fn column_schema_count(backend: &khive_db::StorageBackend, table: &str, column: &str) -> i64 {
    backend
        .pool()
        .reader()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM pragma_table_xinfo(?1, 'main') WHERE name = ?2",
            [table, column],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn pack_column_upgrades_preserve_owner_metadata_and_apply_on_shared_backend() {
    let backend = khive_db::StorageBackend::memory().unwrap();
    seed_column_schema(&backend);
    let registry = column_schema_registry();
    let plans = registry.all_schema_plans_with_columns();
    assert_eq!(plans.len(), 2);
    let (_, alpha_columns) = plans.iter().find(|(plan, _)| plan.pack == "alpha").unwrap();
    assert_eq!(alpha_columns[0].table, "t_alpha");
    assert_eq!(alpha_columns[0].affinity, PackColumnAffinity::Text);
    let (_, beta_columns) = plans.iter().find(|(plan, _)| plan.pack == "beta").unwrap();
    assert_eq!(beta_columns[0].table, "t_beta");
    assert_eq!(beta_columns[0].affinity, PackColumnAffinity::Integer);

    registry.apply_schema_plans(&backend);
    registry.apply_schema_plans(&backend);
    assert_eq!(column_schema_count(&backend, "t_alpha", "revision"), 1);
    assert_eq!(column_schema_count(&backend, "t_beta", "epoch"), 1);
}

#[test]
fn pack_column_upgrades_follow_assigned_backend_and_default_fallback() {
    let default_backend = khive_db::StorageBackend::memory().unwrap();
    let alpha_backend = khive_db::StorageBackend::memory().unwrap();
    seed_column_schema(&default_backend);
    seed_column_schema(&alpha_backend);
    let registry = column_schema_registry();
    let backend_map = HashMap::from([("alpha", &alpha_backend)]);

    registry
        .apply_schema_plans_with_map(&backend_map, &default_backend)
        .unwrap();
    registry
        .apply_schema_plans_with_map(&backend_map, &default_backend)
        .unwrap();
    assert_eq!(
        column_schema_count(&alpha_backend, "t_alpha", "revision"),
        1
    );
    assert_eq!(
        column_schema_count(&default_backend, "t_alpha", "revision"),
        0
    );
    assert_eq!(column_schema_count(&default_backend, "t_beta", "epoch"), 1);
    assert_eq!(column_schema_count(&alpha_backend, "t_beta", "epoch"), 0);
}

#[test]
fn issue2768_pack_column_upgrades_refuse_read_only_old_schema_without_acquiring_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read_only_column_schema.db");
    {
        let writable = khive_db::StorageBackend::sqlite_for_test(&path).unwrap();
        writable.prepare_core_schema().unwrap();
        seed_column_schema(&writable);
    }
    #[cfg(unix)]
    khive_storage::test_support::freeze_snapshot_sidecars(&path);
    let backend = khive_db::StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let registry = column_schema_registry();
    let writes_before = backend.pool().writer_acquisition_snapshot();

    let error = registry
        .apply_schema_plans_with_map(&HashMap::new(), &backend)
        .unwrap_err()
        .to_string();
    assert!(error.contains("alpha"), "{error}");
    assert!(error.contains("t_alpha.revision"), "{error}");
    assert!(
        error.contains("read-only schema validation failed"),
        "{error}"
    );

    assert_eq!(backend.pool().writer_acquisition_snapshot(), writes_before);
    assert_eq!(column_schema_count(&backend, "t_alpha", "revision"), 0);
    assert_eq!(column_schema_count(&backend, "t_beta", "epoch"), 0);
}

#[test]
fn pack_column_upgrades_reject_cross_pack_addition_ownership_collision() {
    let backend = khive_db::StorageBackend::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "alpha",
        statements: &["CREATE TABLE IF NOT EXISTS t_alpha (id INTEGER)"],
        column_additions: &[],
    }));
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "beta",
        statements: &[],
        column_additions: &[PackColumnAddition {
            table: "t_alpha",
            column: "revision",
            affinity: PackColumnAffinity::Text,
        }],
    }));
    let registry = builder.build().unwrap();
    let error = registry
        .apply_schema_plans_with_map(&HashMap::new(), &backend)
        .unwrap_err();
    assert_eq!(error.pack_a, "alpha");
    assert_eq!(error.pack_b, "beta");
    assert_eq!(error.table, "t_alpha");
    assert_eq!(column_schema_count(&backend, "t_alpha", "revision"), 0);
}

#[test]
fn pack_column_upgrades_do_not_hide_duplicate_create_claims() {
    let backend = khive_db::StorageBackend::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register_boxed(Box::new(SchemaPack {
        pack_name: "alpha",
        statements: &[
            "CREATE TABLE IF NOT EXISTS t_alpha (id INTEGER, revision TEXT)",
            "CREATE TABLE IF NOT EXISTS t_alpha (id INTEGER, revision TEXT)",
        ],
        column_additions: &[PackColumnAddition {
            table: "t_alpha",
            column: "revision",
            affinity: PackColumnAffinity::Text,
        }],
    }));
    let registry = builder.build().unwrap();
    let error = registry
        .apply_schema_plans_with_map(&HashMap::new(), &backend)
        .unwrap_err();
    assert_eq!(error.pack_a, "alpha");
    assert_eq!(error.pack_b, "alpha");
    assert_eq!(error.table, "t_alpha");
    assert_eq!(column_schema_count(&backend, "t_alpha", "revision"), 0);
}
