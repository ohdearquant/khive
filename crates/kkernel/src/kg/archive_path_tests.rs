use super::guard_ndjson_destinations;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let parent = std::env::temp_dir();
        for _ in 0..128 {
            let tick = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("temporary fixture clock must follow UNIX_EPOCH")
                .as_nanos();
            let sequence = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                "khive-archive-path-{}-{tick}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create owned fixture {}: {error}", path.display()),
            }
        }
        panic!("could not exclusively create an owned temporary fixture");
    }

    fn root(&self) -> PathBuf {
        fs::canonicalize(&self.path).expect("canonicalize owned fixture root")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Only a path whose exclusive create_dir succeeded is ever stored here.
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn assert_absent(path: &Path) {
    let error = fs::symlink_metadata(path).expect_err("fixture path must stay absent");
    assert_eq!(error.kind(), io::ErrorKind::NotFound, "{}", path.display());
}

fn assert_refused(db: &Path, entities: &Path, edges: &Path, category: &str) {
    let error = guard_ndjson_destinations(db, entities, edges).unwrap_err();
    assert!(
        error.to_string().contains(category),
        "expected refusal category {category:?}, got {error:#}"
    );
}

#[test]
#[cfg(unix)]
fn canceled_missing_parent_reinspects_db_alias() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = scratch.root();
    let real = root.join("real");
    fs::create_dir(&real).unwrap();
    let db = real.join("source.db");
    fs::write(&db, b"sentinel source, not an opened database").unwrap();
    let alias = root.join("alias");
    symlink("real", &alias).unwrap();
    let entities = root
        .join("missing")
        .join("..")
        .join("alias")
        .join("source.db");
    let edges = root.join("edges.ndjson");
    assert_refused(&db, &entities, &edges, "resolves to source database");
    assert_eq!(
        fs::read(&db).unwrap(),
        b"sentinel source, not an opened database"
    );
    assert_eq!(fs::read_link(&alias).unwrap(), Path::new("real"));
    assert_absent(&root.join("missing"));
    assert_absent(&edges);
}

#[test]
#[cfg(unix)]
fn dangling_ancestor_aliases_are_rejected() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = scratch.root();
    let db = root.join("source.db");
    fs::write(&db, b"source bytes").unwrap();
    let target = root.join("fresh-dir");
    let relative_target = Path::new("not-created").join("..").join("fresh-dir");
    let first = root.join("first");
    let second = root.join("second");
    symlink(&relative_target, &first).unwrap();
    symlink(&target, &second).unwrap();
    assert_refused(
        &db,
        &first.join("leaf"),
        &second.join("leaf"),
        "same destination",
    );
    assert_eq!(fs::read(&db).unwrap(), b"source bytes");
    assert_eq!(fs::read_link(&first).unwrap(), relative_target);
    assert_eq!(fs::read_link(&second).unwrap(), target);
    assert_absent(&root.join("not-created"));
    assert_absent(&target);
}

#[test]
#[cfg(unix)]
fn dangling_leaf_aliases_absent_source() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = scratch.root();
    let db = root.join("fresh.db");
    let entities = root.join("entities.ndjson");
    let edges = root.join("edges.ndjson");
    symlink("fresh.db", &entities).unwrap();
    assert_refused(&db, &entities, &edges, "resolves to source database");
    assert_refused(&db, &edges, &entities, "resolves to source database");
    assert_eq!(fs::read_link(&entities).unwrap(), Path::new("fresh.db"));
    assert_absent(&db);
    assert_absent(&edges);
}

#[test]
fn literal_source_alias_is_rejected() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let db = root.join("source.db");
    let edges = root.join("edges.ndjson");
    fs::write(&db, b"source bytes").unwrap();
    assert_refused(&db, &db, &edges, "resolves to source database");
    assert_refused(&db, &edges, &db, "resolves to source database");
    assert_eq!(fs::read(&db).unwrap(), b"source bytes");
    assert_absent(&edges);
}

#[test]
fn literal_leaf_alias_is_rejected() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let db = root.join("source.db");
    let leaf = root.join("shared.ndjson");
    fs::write(&db, b"source bytes").unwrap();
    assert_refused(&db, &leaf, &leaf, "same destination");
    assert_eq!(fs::read(&db).unwrap(), b"source bytes");
    assert_absent(&leaf);
}

#[test]
#[cfg(unix)]
fn resolved_symlink_aliases_are_rejected() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = scratch.root();
    let db = root.join("source.db");
    let shared = root.join("shared.ndjson");
    let db_alias = root.join("source-alias");
    let first = root.join("first");
    let second = root.join("second");
    let fresh = root.join("fresh.ndjson");
    fs::write(&db, b"source bytes").unwrap();
    fs::write(&shared, b"shared leaf bytes").unwrap();
    symlink(&db, &db_alias).unwrap();
    symlink(&shared, &first).unwrap();
    symlink(&shared, &second).unwrap();
    assert_refused(&db, &db_alias, &fresh, "resolves to source database");
    assert_refused(&db, &fresh, &db_alias, "resolves to source database");
    assert_refused(&db, &first, &second, "same destination");
    assert_eq!(fs::read(&db).unwrap(), b"source bytes");
    assert_eq!(fs::read(&shared).unwrap(), b"shared leaf bytes");
    assert_eq!(fs::read_link(&db_alias).unwrap(), db);
    assert_eq!(fs::read_link(&first).unwrap(), shared);
    assert_eq!(fs::read_link(&second).unwrap(), shared);
    assert_absent(&fresh);
}

#[test]
#[cfg(unix)]
fn source_hardlink_alias_is_rejected() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let db = root.join("source.db");
    let entities = root.join("entities.ndjson");
    let edges = root.join("edges.ndjson");
    fs::write(&db, b"source bytes").unwrap();
    fs::hard_link(&db, &entities).expect("hardlink fixture must be supported, never skipped");
    assert_ne!(
        fs::canonicalize(&db).unwrap(),
        fs::canonicalize(&entities).unwrap()
    );
    assert_refused(
        &db,
        &entities,
        &edges,
        "same physical file as source database",
    );
    assert_refused(
        &db,
        &edges,
        &entities,
        "same physical file as source database",
    );
    assert_eq!(fs::read(&db).unwrap(), b"source bytes");
    assert_eq!(fs::read(&entities).unwrap(), b"source bytes");
    assert_absent(&edges);
}

#[test]
#[cfg(unix)]
fn leaf_hardlink_alias_is_rejected() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let db = root.join("source.db");
    let entities = root.join("entities.ndjson");
    let edges = root.join("edges.ndjson");
    fs::write(&db, b"source bytes").unwrap();
    fs::write(&entities, b"shared leaf bytes").unwrap();
    fs::hard_link(&entities, &edges).expect("hardlink fixture must be supported, never skipped");
    assert_ne!(
        fs::canonicalize(&entities).unwrap(),
        fs::canonicalize(&edges).unwrap()
    );
    assert_refused(&db, &entities, &edges, "are the same physical file");
    assert_eq!(fs::read(&db).unwrap(), b"source bytes");
    assert_eq!(fs::read(&entities).unwrap(), b"shared leaf bytes");
    assert_eq!(fs::read(&edges).unwrap(), b"shared leaf bytes");
}

#[test]
#[cfg(any(unix, windows))]
fn distinct_destinations_preserve_all_originals() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let db = root.join("source.db");
    let entities = root.join("entities.ndjson");
    let edges = root.join("edges.ndjson");
    fs::write(&db, b"source bytes").unwrap();
    fs::write(&entities, b"entity bytes").unwrap();
    fs::write(&edges, b"edge bytes").unwrap();
    guard_ndjson_destinations(&db, &entities, &edges).unwrap();
    let missing_entities = root.join("fresh-entities").join("leaf");
    let missing_edges = root.join("fresh-edges").join("leaf");
    guard_ndjson_destinations(&db, &missing_entities, &missing_edges).unwrap();
    let missing_db = root.join("fresh-source.db");
    guard_ndjson_destinations(&missing_db, &missing_entities, &missing_edges).unwrap();
    assert_absent(&missing_db);
    assert_eq!(fs::read(&db).unwrap(), b"source bytes");
    assert_eq!(fs::read(&entities).unwrap(), b"entity bytes");
    assert_eq!(fs::read(&edges).unwrap(), b"edge bytes");
    assert_absent(&root.join("fresh-entities"));
    assert_absent(&root.join("fresh-edges"));
}

#[test]
#[cfg(not(any(unix, windows)))]
fn unsupported_identity_platform_fails_closed() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let db = root.join("source.db");
    let entities = root.join("entities.ndjson");
    let edges = root.join("edges.ndjson");
    fs::write(&db, b"source bytes").unwrap();
    assert_refused(
        &db,
        &entities,
        &edges,
        "NDJSON export requires physical file identity support on this platform",
    );
    assert_eq!(fs::read(&db).unwrap(), b"source bytes");
    assert_absent(&entities);
    assert_absent(&edges);
}
