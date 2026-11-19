//! Opt-in synthetic sensitivity measurement, not a fleet-distribution claim.
//! Keeps 128 due rows per selector and 16 future legacy rows fixed while only
//! unrelated prefix/typed-property buckets grow. Uses installed schema indexes;
//! no test-only index or ANALYZE is added.

use super::note_filter_page_sql;
use crate::StorageBackend;
use khive_storage::note::{
    FilterOp, NoteExpiryFallback, NoteFilter, NoteTimeOrder, PropertyFilter,
};
use khive_storage::SqlValue;
use rusqlite::StatementStatus;
use serde_json::{json, Value};
use uuid::Uuid;

const AS_OF: i64 = 1_000;
const CREATED_CUTOFF: i64 = 900;
const DUE_PER_SELECTOR: usize = 128;
const FUTURE_LEGACY: usize = 16;
const FIXED: usize = DUE_PER_SELECTOR * 2 + FUTURE_LEGACY;
const BUCKETS: [&str; 12] = [
    "foreign_namespace",
    "foreign_kind",
    "foreign_channel",
    "not_quarantined",
    "other_credential",
    "numeric_quarantined",
    "capitalized_text_quarantined",
    "tab_slug",
    "unicode_space_slug",
    "numeric_slug",
    "exact_slug_tombstone",
    "boolean_slug",
];

#[derive(Clone, Copy)]
enum Quarantined {
    BooleanTrue,
    TextTrue,
    CapitalizedText,
    False,
    NumericOne,
}

#[derive(Clone, Copy)]
enum Slug {
    Missing,
    Null,
    Text(&'static str),
    NumericZero,
    BooleanFalse,
}

struct Seeded {
    id: String,
    namespace: &'static str,
    kind: &'static str,
    channel: &'static str,
    quarantined: Quarantined,
    slug: Slug,
    created: i64,
    expiry: Option<i64>,
    deleted: Option<i64>,
}

impl Seeded {
    fn new(ordinal: usize) -> Self {
        Self {
            id: Uuid::from_u128(ordinal as u128 + 1).to_string(),
            namespace: "local",
            kind: "message",
            channel: "email",
            quarantined: if ordinal % 4 < 2 {
                Quarantined::BooleanTrue
            } else {
                Quarantined::TextTrue
            },
            slug: Slug::Missing,
            created: 1,
            expiry: Some(1),
            deleted: None,
        }
    }

    fn properties(&self) -> Value {
        let mut value = json!({
            "channel_kind": self.channel,
            "quarantined": match self.quarantined {
                Quarantined::BooleanTrue => json!(true),
                Quarantined::TextTrue => json!("true"),
                Quarantined::CapitalizedText => json!("True"),
                Quarantined::False => json!(false),
                Quarantined::NumericOne => json!(1),
            },
        });
        let slug = match self.slug {
            Slug::Missing => None,
            Slug::Null => Some(Value::Null),
            Slug::Text(text) => Some(json!(text)),
            Slug::NumericZero => Some(json!(0)),
            Slug::BooleanFalse => Some(json!(false)),
        };
        if let Some(slug) = slug {
            value["channel_slug"] = slug;
        }
        value
    }
}

fn seeded_rows(total: usize) -> (Vec<Seeded>, [usize; 12]) {
    let slugs = [Slug::Missing, Slug::Null, Slug::Text(""), Slug::Text("   ")];
    let mut rows = Vec::with_capacity(total);
    for n in 0..DUE_PER_SELECTOR {
        let mut row = Seeded::new(rows.len());
        row.slug = Slug::Text("inbox");
        row.created = AS_OF + 100;
        row.expiry = Some(AS_OF - (n % 4) as i64);
        rows.push(row);
    }
    for n in 0..DUE_PER_SELECTOR {
        let mut row = Seeded::new(rows.len());
        row.slug = slugs[n % slugs.len()];
        row.created = if n % 8 == 0 {
            CREATED_CUTOFF
        } else {
            CREATED_CUTOFF - 8 + (n % 8) as i64
        };
        if n % 2 == 0 {
            row.expiry = None;
        } else {
            row.created = AS_OF + 100;
            row.expiry = Some(AS_OF - ((n / 2) % 4) as i64);
        }
        row.deleted = (n % 3 == 0).then_some(500);
        rows.push(row);
    }
    // Same-bucket future rows stay fixed: eight explicit future expiries,
    // eight recent NULL-expiry fallbacks. This does not measure their growth.
    for n in 0..FUTURE_LEGACY {
        let mut row = Seeded::new(rows.len());
        row.slug = slugs[n % slugs.len()];
        row.created = CREATED_CUTOFF + 1 + (n % 8) as i64;
        row.expiry = (n < 8).then_some(AS_OF + 1 + n as i64);
        row.deleted = (n % 3 == 0).then_some(500);
        rows.push(row);
    }
    let mut counts = [0; 12];
    for n in 0..total - FIXED {
        let mut row = Seeded::new(rows.len());
        row.expiry = (n % 2 == 0).then_some(1);
        row.deleted = (n % 3 == 0).then_some(2);
        let bucket = n % BUCKETS.len();
        counts[bucket] += 1;
        match bucket {
            0 => row.namespace = "foreign",
            1 => row.kind = "observation",
            2 => row.channel = "telegram",
            3 => row.quarantined = Quarantined::False,
            4 => row.slug = Slug::Text("other-credential"),
            5 => row.quarantined = Quarantined::NumericOne,
            6 => row.quarantined = Quarantined::CapitalizedText,
            7 => row.slug = Slug::Text("\t"),
            8 => row.slug = Slug::Text("\u{2003}"),
            9 => row.slug = Slug::NumericZero,
            10 => {
                row.slug = Slug::Text("inbox");
                row.expiry = Some(1);
                row.deleted = Some(2);
            }
            _ => row.slug = Slug::BooleanFalse,
        }
        rows.push(row);
    }
    (rows, counts)
}

#[derive(Clone, Copy)]
enum Selector {
    Exact,
    Legacy,
}

impl Selector {
    fn name(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Legacy => "legacy_slugless",
        }
    }

    fn filter(self) -> NoteFilter {
        let mut filter = NoteFilter {
            kind: Some("message".into()),
            property_filters: vec![
                PropertyFilter {
                    json_path: "$.channel_kind".into(),
                    op: FilterOp::Eq,
                    value: SqlValue::Text("email".into()),
                },
                PropertyFilter {
                    json_path: "$.quarantined".into(),
                    op: FilterOp::TrueOrTextTrue,
                    value: SqlValue::Null,
                },
            ],
            ..Default::default()
        };
        let op = match self {
            Self::Exact => {
                filter.max_expires_at = Some(AS_OF);
                filter.time_order = Some(NoteTimeOrder::ExpiresAt);
                FilterOp::Eq
            }
            Self::Legacy => {
                filter.include_deleted = true;
                filter.expiry_fallback = Some(NoteExpiryFallback {
                    expires_at_or_before: AS_OF,
                    created_at_or_before: CREATED_CUTOFF,
                });
                filter.time_order = Some(NoteTimeOrder::ExpiresAtOrCreatedAt);
                FilterOp::MissingNullOrSpaceEmptyText
            }
        };
        filter.property_filters.push(PropertyFilter {
            json_path: "$.channel_slug".into(),
            op,
            value: match self {
                Self::Exact => SqlValue::Text("inbox".into()),
                Self::Legacy => SqlValue::Null,
            },
        });
        filter
    }
}

// Independent evaluation of seeded fields: no query builder or emitted SQL
// predicates are reused here. SQLite trim's default removes ASCII spaces only.
fn oracle(rows: &[Seeded], selector: Selector) -> Vec<String> {
    let mut due = Vec::new();
    for row in rows {
        if row.namespace != "local"
            || row.kind != "message"
            || row.channel != "email"
            || !matches!(
                row.quarantined,
                Quarantined::BooleanTrue | Quarantined::TextTrue
            )
        {
            continue;
        }
        let eligible = match selector {
            Selector::Exact => {
                row.deleted.is_none()
                    && matches!(row.slug, Slug::Text("inbox"))
                    && row.expiry.is_some_and(|expiry| expiry <= AS_OF)
            }
            Selector::Legacy => {
                matches!(row.slug, Slug::Missing | Slug::Null)
                    || matches!(row.slug, Slug::Text(text) if text.trim_matches(' ').is_empty())
            }
        };
        let expiry_ok = match selector {
            Selector::Exact => true,
            Selector::Legacy => match row.expiry {
                Some(expiry) => expiry <= AS_OF,
                None => row.created <= CREATED_CUTOFF,
            },
        };
        if eligible && expiry_ok {
            due.push((row.expiry.unwrap_or(row.created), row.id.clone()));
        }
    }
    due.sort_unstable();
    due.into_iter().take(128).map(|(_, id)| id).collect()
}

#[test]
#[ignore = "opt-in synthetic cleanup measurement; run with --ignored --nocapture"]
fn measure_comm_cleanup_selectors_over_growing_unrelated_corpus() {
    for total in [1_000, 10_000, 100_000] {
        let (rows, noise_counts) = seeded_rows(total);
        let backend = StorageBackend::memory().unwrap();
        let schema_version = backend.prepare_core_schema().unwrap();
        let _notes = backend.notes().unwrap();
        backend.pool().writer().unwrap().transaction(|conn| {
            let mut insert = conn.prepare(
                "INSERT INTO notes(id, namespace, kind, properties, created_at, updated_at, expires_at, deleted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7)",
            )?;
            for row in rows.iter().rev() {
                let properties = row.properties();
                insert.execute(rusqlite::params![row.id, row.namespace, row.kind,
                    properties.to_string(), row.created, row.expiry, row.deleted])?;
            }
            Ok(())
        }).unwrap();
        for selector in [Selector::Exact, Selector::Legacy] {
            let reader = backend.pool().reader().unwrap();
            let conn = reader.conn();
            let indexes = conn.prepare("SELECT name FROM sqlite_schema WHERE type='index' AND tbl_name='notes' ORDER BY name")
                .unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap()
                .collect::<rusqlite::Result<Vec<_>>>().unwrap();
            let (sql, params) = note_filter_page_sql("local", &selector.filter(), 128, 0).unwrap();
            let params: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            let plans = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(params.as_slice(), |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let mut statement = conn.prepare(&sql).unwrap();
            let ids = statement
                .query_map(params.as_slice(), |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                ids,
                oracle(&rows, selector),
                "{} N={total}",
                selector.name()
            );
            let distribution: Vec<_> = BUCKETS.iter().zip(noise_counts).collect();
            eprintln!(
                "COMM_CLEANUP_MEASUREMENT {}",
                json!({
                    "selector": selector.name(), "N": total, "seeded": rows.len(), "selected": ids.len(),
                    "schema_version": schema_version, "indexes": indexes,
                    "fixed_exact_due_live": DUE_PER_SELECTOR, "fixed_legacy_due": DUE_PER_SELECTOR,
                    "fixed_legacy_future": FUTURE_LEGACY, "growing_buckets": distribution,
                    "total_tombstones": rows.iter().filter(|row| row.deleted.is_some()).count(),
                    "total_null_expiries": rows.iter().filter(|row| row.expiry.is_none()).count(),
                    "plans": plans, "vm_steps": statement.get_status(StatementStatus::VmStep),
                    "sorts": statement.get_status(StatementStatus::Sort), "explicit_analyze": false,
                    "pool_max_readers": backend.pool().max_readers(),
                })
            );
        }
    }
}
