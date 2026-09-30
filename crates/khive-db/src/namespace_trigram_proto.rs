//! Experimental FTS5 tokenizer for the namespace-bounded trigram prototype.
//!
//! The caller supplies a trusted, fixed three-codepoint PUA envelope for
//! every indexed field and query phrase. The wrapped SQLite trigram tokenizer
//! sees only the remaining text. Its emitted tokens are prefixed with the
//! envelope, so a foreign row containing another namespace's key in its body
//! cannot produce that namespace's postings. No numbered migration uses this.

use rusqlite::ffi;
use rusqlite::Connection;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr;
use std::slice;

const KEY_BYTES: usize = 9;
const TOKEN_BYTES: usize = 32;
const SLOT_RADIX: u64 = 6_400;
const MAX_SLOT_EXCLUSIVE: u64 = SLOT_RADIX * SLOT_RADIX * SLOT_RADIX;
const PUA_BASE: u32 = 0xE000;

pub fn key_for_slot(slot: u64) -> Option<String> {
    if !(1..MAX_SLOT_EXCLUSIVE).contains(&slot) {
        return None;
    }
    let codepoints = [
        PUA_BASE + (slot % SLOT_RADIX) as u32,
        PUA_BASE + ((slot / SLOT_RADIX) % SLOT_RADIX) as u32,
        PUA_BASE + ((slot / (SLOT_RADIX * SLOT_RADIX)) % SLOT_RADIX) as u32,
    ];
    codepoints.into_iter().map(char::from_u32).collect()
}

fn split_envelope(input: &[u8]) -> Option<([u8; KEY_BYTES], &[u8])> {
    let prefix = input.get(..KEY_BYTES)?;
    let key = std::str::from_utf8(prefix).ok()?;
    if key.chars().count() != 3
        || !key
            .chars()
            .all(|ch| ('\u{e000}'..='\u{f8ff}').contains(&ch))
        || key == "\u{e000}\u{e000}\u{e000}"
    {
        return None;
    }
    let mut bytes = [0; KEY_BYTES];
    bytes.copy_from_slice(prefix);
    Some((bytes, &input[KEY_BYTES..]))
}

pub fn envelope(key: &str, text: &str) -> Option<String> {
    let (_, suffix) = split_envelope(key.as_bytes())?;
    if !suffix.is_empty() {
        return None;
    }
    Some(format!("{key}{text}"))
}

pub fn scoped_match(key: &str, term: &str) -> Option<String> {
    let input = envelope(key, term)?.replace('"', "\"\"");
    Some(format!("{{slug name content}} : \"{input}\""))
}

type TokenCallback =
    unsafe extern "C" fn(*mut c_void, c_int, *const c_char, c_int, c_int, c_int) -> c_int;

struct Registration {
    inner: ffi::fts5_tokenizer,
    inner_context: *mut c_void,
}

struct Instance {
    inner: ffi::fts5_tokenizer,
    inner_instance: *mut ffi::Fts5Tokenizer,
}

struct Forward {
    key: [u8; KEY_BYTES],
    context: *mut c_void,
    callback: TokenCallback,
}

unsafe extern "C" fn forward_token(
    context: *mut c_void,
    flags: c_int,
    token: *const c_char,
    token_len: c_int,
    start: c_int,
    end: c_int,
) -> c_int {
    if context.is_null() || token.is_null() || token_len < 0 {
        return ffi::SQLITE_ERROR;
    }
    let forward = unsafe { &mut *context.cast::<Forward>() };
    let len = token_len as usize;
    if len > TOKEN_BYTES - KEY_BYTES {
        return ffi::SQLITE_TOOBIG;
    }
    let mut prefixed = [0u8; TOKEN_BYTES];
    prefixed[..KEY_BYTES].copy_from_slice(&forward.key);
    prefixed[KEY_BYTES..KEY_BYTES + len]
        .copy_from_slice(unsafe { slice::from_raw_parts(token.cast::<u8>(), len) });
    unsafe {
        (forward.callback)(
            forward.context,
            flags,
            prefixed.as_ptr().cast::<c_char>(),
            (KEY_BYTES + len) as c_int,
            start + KEY_BYTES as c_int,
            end + KEY_BYTES as c_int,
        )
    }
}

unsafe extern "C" fn create(
    context: *mut c_void,
    _args: *mut *const c_char,
    arg_count: c_int,
    output: *mut *mut ffi::Fts5Tokenizer,
) -> c_int {
    if context.is_null() || output.is_null() || arg_count != 0 {
        return ffi::SQLITE_ERROR;
    }
    let registration = unsafe { &*context.cast::<Registration>() };
    let Some(inner_create) = registration.inner.xCreate else {
        return ffi::SQLITE_ERROR;
    };
    let mut args: [*const c_char; 2] = [c"case_sensitive".as_ptr(), c"0".as_ptr()];
    let mut inner_instance = ptr::null_mut();
    let rc = unsafe {
        inner_create(
            registration.inner_context,
            args.as_mut_ptr(),
            args.len() as c_int,
            &mut inner_instance,
        )
    };
    if rc != ffi::SQLITE_OK {
        return rc;
    }
    let instance = Box::new(Instance {
        inner: registration.inner,
        inner_instance,
    });
    unsafe { *output = Box::into_raw(instance).cast() };
    ffi::SQLITE_OK
}

unsafe extern "C" fn delete(instance: *mut ffi::Fts5Tokenizer) {
    if instance.is_null() {
        return;
    }
    let instance = unsafe { Box::from_raw(instance.cast::<Instance>()) };
    if let Some(inner_delete) = instance.inner.xDelete {
        unsafe { inner_delete(instance.inner_instance) };
    }
}

unsafe extern "C" fn tokenize(
    instance: *mut ffi::Fts5Tokenizer,
    context: *mut c_void,
    flags: c_int,
    input: *const c_char,
    input_len: c_int,
    callback: Option<TokenCallback>,
) -> c_int {
    if instance.is_null() || input.is_null() || input_len < KEY_BYTES as c_int {
        return ffi::SQLITE_ERROR;
    }
    let Some(callback) = callback else {
        return ffi::SQLITE_ERROR;
    };
    let instance = unsafe { &*instance.cast::<Instance>() };
    let Some(inner_tokenize) = instance.inner.xTokenize else {
        return ffi::SQLITE_ERROR;
    };
    let bytes = unsafe { slice::from_raw_parts(input.cast::<u8>(), input_len as usize) };
    let Some((key, body)) = split_envelope(bytes) else {
        return ffi::SQLITE_ERROR;
    };
    let mut forward = Forward {
        key,
        context,
        callback,
    };
    unsafe {
        inner_tokenize(
            instance.inner_instance,
            (&mut forward as *mut Forward).cast::<c_void>(),
            flags,
            body.as_ptr().cast::<c_char>(),
            body.len() as c_int,
            Some(forward_token),
        )
    }
}

unsafe extern "C" fn destroy(context: *mut c_void) {
    if !context.is_null() {
        drop(unsafe { Box::from_raw(context.cast::<Registration>()) });
    }
}

fn sqlite_message(db: *mut ffi::sqlite3, operation: &str, rc: c_int) -> String {
    let message = unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(db)) }
        .to_string_lossy()
        .into_owned();
    format!("{operation} failed ({rc}): {message}")
}

pub fn register(connection: &Connection) -> Result<(), String> {
    let db = unsafe { connection.handle() };
    let mut statement = ptr::null_mut();
    let rc = unsafe {
        ffi::sqlite3_prepare_v2(
            db,
            c"SELECT fts5(?1)".as_ptr(),
            -1,
            &mut statement,
            ptr::null_mut(),
        )
    };
    if rc != ffi::SQLITE_OK {
        return Err(sqlite_message(db, "prepare FTS5 API lookup", rc));
    }
    let mut api: *mut ffi::fts5_api = ptr::null_mut();
    let rc = unsafe {
        ffi::sqlite3_bind_pointer(
            statement,
            1,
            (&mut api as *mut *mut ffi::fts5_api).cast::<c_void>(),
            c"fts5_api_ptr".as_ptr(),
            None,
        )
    };
    let step = if rc == ffi::SQLITE_OK {
        unsafe { ffi::sqlite3_step(statement) }
    } else {
        rc
    };
    unsafe { ffi::sqlite3_finalize(statement) };
    if step != ffi::SQLITE_ROW && step != ffi::SQLITE_DONE {
        return Err(sqlite_message(db, "resolve FTS5 API", step));
    }
    if api.is_null() {
        return Err("SQLite did not expose the FTS5 API pointer".into());
    }
    let Some(find) = (unsafe { &*api }).xFindTokenizer else {
        return Err("SQLite FTS5 has no xFindTokenizer".into());
    };
    let Some(create_tokenizer) = (unsafe { &*api }).xCreateTokenizer else {
        return Err("SQLite FTS5 has no xCreateTokenizer".into());
    };
    let mut inner_context = ptr::null_mut();
    let mut inner = ffi::fts5_tokenizer {
        xCreate: None,
        xDelete: None,
        xTokenize: None,
    };
    let rc = unsafe { find(api, c"trigram".as_ptr(), &mut inner_context, &mut inner) };
    if rc != ffi::SQLITE_OK {
        return Err(sqlite_message(db, "find built-in trigram tokenizer", rc));
    }
    let registration = Box::new(Registration {
        inner,
        inner_context,
    });
    let context = Box::into_raw(registration).cast::<c_void>();
    let mut wrapper = ffi::fts5_tokenizer {
        xCreate: Some(create),
        xDelete: Some(delete),
        xTokenize: Some(tokenize),
    };
    let rc = unsafe {
        create_tokenizer(
            api,
            c"namespace_trigram_v1".as_ptr(),
            context,
            &mut wrapper,
            Some(destroy),
        )
    };
    if rc != ffi::SQLITE_OK {
        unsafe { destroy(context) };
        return Err(sqlite_message(
            db,
            "register namespace trigram tokenizer",
            rc,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{envelope, key_for_slot, register, scoped_match, split_envelope};
    use rusqlite::{params, Connection};

    #[test]
    fn slots_are_distinct_and_envelopes_do_not_reparse_embedded_keys() {
        let a = key_for_slot(1).expect("slot A");
        let b = key_for_slot(2).expect("slot B");
        assert_ne!(a, b);
        assert_eq!(key_for_slot(0), None);
        assert_eq!(key_for_slot(6_400u64.pow(3)), None);
        assert!(envelope("\u{e000}\u{e000}\u{e000}", "invalid slot").is_none());
        let text = envelope(&b, &format!("{a} zznamespaceguard")).expect("B envelope");
        let (found, body) = split_envelope(text.as_bytes()).expect("valid envelope");
        assert_eq!(found.as_slice(), b.as_bytes());
        assert_eq!(body, format!("{a} zznamespaceguard").as_bytes());
        assert!(split_envelope(b"ordinary text").is_none());
    }

    #[test]
    fn foreign_body_cannot_spoof_another_slot_posting() {
        let conn = Connection::open_in_memory().expect("open SQLite");
        register(&conn).expect("register tokenizer");
        conn.execute_batch(
            "CREATE VIRTUAL TABLE proto USING fts5(\
                slug, name, content, tokenize='namespace_trigram_v1'\
            )",
        )
        .expect("create feature-gated FTS table");

        let a = key_for_slot(1).expect("A key");
        let b = key_for_slot(2).expect("B key");
        let term = "zznamespaceguard";
        conn.execute(
            "INSERT INTO proto(rowid, slug, name, content) VALUES(1, ?1, ?2, ?3)",
            params![
                envelope(&b, "b-slug").unwrap(),
                envelope(&b, "B name").unwrap(),
                envelope(&b, &format!("{a} {term}")).unwrap(),
            ],
        )
        .expect("insert adversarial B first");

        let a_match = scoped_match(&a, term).expect("A query");
        let a_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM proto WHERE proto MATCH ?1",
                [&a_match],
                |row| row.get(0),
            )
            .expect("query absent A");
        assert_eq!(a_count, 0, "foreign body text must not create A postings");

        conn.execute(
            "INSERT INTO proto(rowid, slug, name, content) VALUES(2, ?1, ?2, ?3)",
            params![
                envelope(&a, "a-slug").unwrap(),
                envelope(&a, "A name").unwrap(),
                envelope(&a, term).unwrap(),
            ],
        )
        .expect("insert A");
        let a_rowid: i64 = conn
            .query_row(
                "SELECT rowid FROM proto WHERE proto MATCH ?1",
                [&a_match],
                |row| row.get(0),
            )
            .expect("query A");
        assert_eq!(a_rowid, 2);

        assert!(conn
            .execute(
                "INSERT INTO proto(rowid, slug, name, content) VALUES(3, 'plain', 'plain', 'plain')",
                [],
            )
            .is_err());
    }
}
