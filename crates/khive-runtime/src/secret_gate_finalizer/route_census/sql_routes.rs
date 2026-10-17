use super::*;

pub(super) const STORE_WRITES: &[&str] = &[
    "upsert_entity",
    "upsert_entities",
    "upsert_entity_with_attachments",
    "insert_entity_if_absent",
    "replace_entity_if_unchanged",
    "upsert_note",
    "upsert_notes",
    "insert_note_if_absent",
    "try_insert_note",
    "try_insert_note_with_attachments",
    "replace_note_if_unchanged",
    "update_note_properties",
    "set_note_property",
    "try_patch_note_property",
    "patch_note_property_atomic",
    "try_patch_note_properties",
];

// The complement is explicit, including readers: a new trait method of any
// spelling makes the census red until its properties behavior is classified.
pub(super) const NON_PROPERTIES_STORE_METHODS: &[&str] = &[
    "get_entity",
    "delete_entity",
    "query_entities",
    "query_entities_count_free",
    "entity_sequence",
    "query_entities_after",
    "count_entities",
    "count_entities_by_type",
    "get_entity_including_deleted",
    "get_live_notes_by_key",
    "query_keyed_notes",
    "get_note",
    "get_note_including_deleted",
    "delete_note",
    "query_notes",
    "query_notes_count_free",
    "query_notes_filtered",
    "query_notes_filtered_count_free",
    "count_notes_filtered_in_snapshot",
    "count_notes_filtered_bounded_in_snapshot",
    "note_sequence",
    "query_notes_filtered_after",
    "query_notes_filtered_bounded",
    "count_notes",
    "count_notes_in_namespaces",
    "get_notes_batch",
    "get_notes_batch_including_deleted",
    "get_note_visibility_batch",
];

pub(super) const ENTITY_BUILDERS: &[&str] = &[
    "entity_upsert_statement",
    "entity_insert_if_absent_statement",
    "entity_replace_if_unchanged_statement",
];
pub(super) const NOTE_BUILDERS: &[&str] = &[
    "note_insert_if_absent_statement",
    "note_insert_keyed_statement",
    "note_upsert_statement",
    "note_replace_if_unchanged_statement",
    "note_update_properties_statement",
    "note_set_property_statement",
];

// External references to these khive-db statements bypass the builder and
// store-method detectors. Both statements insert a whole note properties
// object, even when an ON CONFLICT clause does not replace an existing row.
pub(super) const NOTE_PROPERTY_SQL_CONSTANTS: &[&str] =
    &["NOTE_UPSERT_SQL", "NOTE_INSERT_IF_ABSENT_SQL"];

// Evidence for a site that reaches a note SQL constant only if a name that the
// census cannot classify is not a type. The census refuses it wherever it
// occurs, instead of routing or dropping it.
pub(super) const UNRESOLVED_CONSTANT: &str = "UNRESOLVED note SQL constant";

// Evidence prefix for a path that resolves to a note SQL constant.
pub(super) const SQL_CONSTANT_EVIDENCE: &str = "SQL constant ";

// Evidence prefix for a statement that writes a guarded table in a spelling the
// census cannot classify. The census refuses it wherever it occurs, because a
// spelling it cannot read is a route it cannot inventory.
pub(super) const UNCLASSIFIED_SQL: &str = "UNCLASSIFIED guarded-table SQL write";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DetectedClass {
    WholeObject,
    SingleKey(String),
    FixedKeySet(BTreeSet<String>),
}

pub(super) fn canonical_key_path(path: &str) -> Option<String> {
    // Canonicalization sorts and deduplicates the set; labels remain byte-exact.
    bare_top_level_path(path).then(|| path.to_owned())
}

pub(super) fn detected_keys(class: &DetectedClass) -> Option<BTreeSet<String>> {
    match class {
        DetectedClass::WholeObject => None,
        DetectedClass::SingleKey(path) => Some(BTreeSet::from([canonical_key_path(path)?])),
        DetectedClass::FixedKeySet(paths) => Some(paths.clone()),
    }
}

pub(super) fn detected_key_class(paths: BTreeSet<String>) -> DetectedClass {
    if paths.len() == 1 {
        DetectedClass::SingleKey(paths.into_iter().next().expect("nonempty key set"))
    } else {
        DetectedClass::FixedKeySet(paths)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RouteClass {
    Application,
    Migration,
}

#[derive(Debug, Clone)]
pub(super) struct Site {
    pub(super) key: String,
    pub(super) target: Substrate,
    pub(super) route_class: RouteClass,
    pub(super) class: DetectedClass,
    pub(super) write_count: usize,
    pub(super) calls: BTreeSet<String>,
    pub(super) evidence: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub(super) struct RuntimeTableSite {
    pub(super) key: String,
    pub(super) write_count: usize,
    pub(super) calls: BTreeSet<String>,
    pub(super) evidence: BTreeSet<String>,
}

#[derive(Debug)]
pub(super) struct SourcePopulation {
    pub(super) properties: Vec<Site>,
    pub(super) runtime_tables: Vec<RuntimeTableSite>,
    /// SQL assets that a followed loader resolved, for the asset-side reach check.
    pub(super) resolved_sql: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SqlConstantReference {
    pub(super) site: String,
    pub(super) constant: String,
}

pub(super) struct ScannedSource {
    pub(super) sql_errors: Vec<String>,
    pub(super) resolved_sql: BTreeSet<String>,
    pub(super) sites: Vec<Site>,
    pub(super) runtime_tables: Vec<RuntimeTableSite>,
    /// Every expression path has an ordinal, whether or not it resolves. Both
    /// passes visit the same AST, so a sibling reference cannot stand in for it.
    pub(super) constant_references: BTreeMap<usize, SqlConstantReference>,
}

pub(super) fn attribute_name(attr: &Attribute) -> String {
    attr.path()
        .segments
        .iter()
        .map(|part| part.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

pub(super) fn test_only(attrs: &[Attribute]) -> bool {
    fn requires_test(meta: &syn::Meta) -> bool {
        match meta {
            syn::Meta::Path(path) => path.is_ident("test"),
            syn::Meta::List(list) if list.path.is_ident("all") => list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .is_ok_and(|items| items.iter().any(requires_test)),
            syn::Meta::List(list) if list.path.is_ident("any") => list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .is_ok_and(|items| !items.is_empty() && items.iter().all(requires_test)),
            _ => false,
        }
    }

    attrs.iter().any(|attr| {
        let name = attribute_name(attr);
        if name == "test" || name == "tokio::test" {
            return true;
        }
        name == "cfg"
            && matches!(&attr.meta, syn::Meta::List(list)
                if list.parse_args::<syn::Meta>().is_ok_and(|meta| requires_test(&meta)))
    })
}

/// One bare top-level identifier, optionally following `$` and `.`.
pub(super) fn bare_top_level_path(path: &str) -> bool {
    let segment = path.strip_prefix("$.").unwrap_or(path);
    if segment == "$" || segment.is_empty() || segment.contains('.') {
        return false;
    }
    let mut bytes = segment.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub(super) fn literal_string(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Str(value),
            ..
        }) => Some(value.value()),
        Expr::Reference(reference) => literal_string(&reference.expr),
        Expr::Paren(paren) => literal_string(&paren.expr),
        _ => None,
    }
}

/// The guarded table that one word names, if any.
pub(super) fn guarded_table(word: &str) -> Option<Substrate> {
    match word {
        "ENTITIES" => Some(Substrate::Entity),
        "NOTES" => Some(Substrate::Note),
        _ => None,
    }
}

/// Find the guarded table at the front of a write target. The flag says whether
/// it was reached bare or through the `main` or `temp` schema, and the count is
/// how many words the table and its schema took.
pub(super) fn guarded_head(head: &[&str]) -> Option<(Substrate, bool, usize)> {
    if let Some(target) = head.first().copied().and_then(guarded_table) {
        return Some((target, true, 1));
    }
    let target = head.get(1).copied().and_then(guarded_table)?;
    Some((target, matches!(head[0], "MAIN" | "TEMP"), 2))
}

/// What may stand between an UPDATE's table and its SET: an optional alias and
/// an optional index hint.
pub(super) fn update_tail_is_known(tail: &[&str]) -> bool {
    let rest = match tail {
        ["AS", _, rest @ ..] => rest,
        [alias, rest @ ..] if !matches!(*alias, "INDEXED" | "NOT") => rest,
        rest => rest,
    };
    matches!(rest, [] | ["NOT", "INDEXED"] | ["INDEXED", "BY", _])
}

/// A literal names one substrate, and entities win a tie as they always have.
pub(super) fn entity_first(targets: &[Substrate]) -> Option<Substrate> {
    targets
        .iter()
        .copied()
        .find(|target| *target == Substrate::Entity)
        .or_else(|| targets.first().copied())
}

/// Read every insert, replace and update statement in a literal that targets a
/// guarded table. The first collection contains one substrate per classified
/// write occurrence. The second contains one substrate per occurrence that
/// names a guarded table in a spelling the census cannot read: the census must
/// refuse it, because a write it cannot classify is a route it cannot inventory.
pub(super) fn sql_write_occurrences(literal: &str) -> (Vec<Substrate>, Vec<Substrate>) {
    // Match SQL tokens rather than keeping SQL-shaped matcher literals in this
    // census: other source censuses must not mistake those for writer sites.
    // Comments are not SQL, so they are dropped before the words are read, and
    // each parenthesis becomes a word of its own so that a column list is never
    // read as part of the write target.
    let spaced = sql_statements(literal)
        .join(" ")
        .to_ascii_uppercase()
        .replace('(', " ( ");
    let words = spaced
        .split(|character: char| {
            !character.is_ascii_alphanumeric() && character != '_' && character != '('
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    let mut routes = Vec::new();
    let mut unclassified = Vec::new();
    for (index, word) in words.iter().enumerate() {
        // INSERT OR REPLACE is one write, not an INSERT plus a REPLACE.
        if *word == "REPLACE" && index > 0 && words[index - 1] == "OR" {
            continue;
        }
        let mut at = index + 1;
        match *word {
            // REPLACE only starts a statement when INTO follows it.
            "INSERT" | "REPLACE" => {
                if *word == "INSERT" && words.get(at) == Some(&"OR") {
                    at += 2;
                }
                if words.get(at) != Some(&"INTO") {
                    continue;
                }
                let head = words[at + 1..]
                    .iter()
                    .copied()
                    .take_while(|candidate| {
                        !matches!(*candidate, "(" | "VALUES" | "SELECT" | "DEFAULT" | "WITH")
                    })
                    .take(4)
                    .collect::<Vec<_>>();
                match guarded_head(&head) {
                    Some((target, true, _)) => routes.push(target),
                    _ => unclassified.extend(head.iter().take(2).copied().find_map(guarded_table)),
                }
            }
            "UPDATE" => {
                if words.get(at) == Some(&"OR") {
                    at += 2;
                }
                let rest = words.get(at..).unwrap_or_default();
                let Some(set_at) = rest
                    .iter()
                    .position(|candidate| matches!(*candidate, "SET" | "("))
                else {
                    continue;
                };
                if rest[set_at] != "SET" {
                    continue;
                }
                let head = &rest[..set_at];
                let after_set = &rest[set_at + 1..];
                // A trigger header names its table after ON, never directly
                // after UPDATE, and no UPDATE target is longer than a schema,
                // a table, an alias and an index hint.
                if matches!(head.first().copied(), Some("ON" | "OF")) || head.len() > 7 {
                    continue;
                }
                // An update counts when PROPERTIES appears anywhere after SET.
                // Cutting the SET clause at the first WHERE would miss a write
                // that follows a subquery or a string containing that word, so
                // an update that only reads properties is reported as well.
                if !after_set.contains(&"PROPERTIES") {
                    continue;
                }
                match guarded_head(head) {
                    Some((target, true, used)) if update_tail_is_known(&head[used..]) => {
                        routes.push(target);
                    }
                    Some((target, _, _)) => unclassified.push(target),
                    None => {}
                }
            }
            _ => {}
        }
    }
    (routes, unclassified)
}

pub(super) fn sql_write_shapes(literal: &str) -> (Option<Substrate>, Option<Substrate>) {
    let (routes, unclassified) = sql_write_occurrences(literal);
    (entity_first(&routes), entity_first(&unclassified))
}

pub(super) fn sql_target(literal: &str) -> Option<Substrate> {
    sql_write_shapes(literal).0
}

pub(super) fn sql_unclassified_write(literal: &str) -> Option<Substrate> {
    sql_write_shapes(literal).1
}

/// Count table-position placeholders without resolving their runtime values.
pub(super) fn runtime_table_write_count(literal: &str) -> usize {
    let mut count = 0;
    for statement in sql_statements(literal) {
        // The flag distinguishes SQL words from quoted text and placeholders,
        // so an UPDATE inside a value string is not a statement keyword.
        let mut tokens: Vec<(String, bool)> = Vec::new();
        let mut chars = statement.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch.is_whitespace() {
                continue;
            }
            if ch.is_ascii_alphanumeric() || ch == '_' {
                let mut word = String::from(ch);
                while chars
                    .peek()
                    .is_some_and(|next| next.is_ascii_alphanumeric() || *next == '_')
                {
                    word.push(chars.next().expect("peeked word"));
                }
                tokens.push((word.to_ascii_uppercase(), true));
            } else if matches!(ch, '\'' | '"' | '`' | '[' | '{') {
                let closing = match ch {
                    '[' => ']',
                    '{' => '}',
                    _ => ch,
                };
                let mut value = String::from(ch);
                while let Some(next) = chars.next() {
                    value.push(next);
                    if next == closing {
                        if ch != '{' && chars.peek() == Some(&closing) {
                            value.push(chars.next().expect("peeked escaped quote"));
                        } else {
                            break;
                        }
                    }
                }
                tokens.push((value, false));
            } else {
                tokens.push((ch.to_string(), false));
            }
        }
        let word_at = |at: usize, word: &str| {
            tokens
                .get(at)
                .is_some_and(|(actual, is_word)| *is_word && actual == word)
        };
        let placeholder_at = |at: usize| {
            tokens
                .get(at)
                .is_some_and(|(token, _)| token.contains('{') && token.contains('}'))
                || (tokens.get(at + 1).is_some_and(|(token, _)| token == ".")
                    && tokens
                        .get(at + 2)
                        .is_some_and(|(token, _)| token.contains('{') && token.contains('}')))
        };
        for at in 0..tokens.len() {
            let insert = word_at(at, "INSERT");
            let replace = word_at(at, "REPLACE") && (at == 0 || !word_at(at - 1, "OR"));
            let is_update = word_at(at, "UPDATE");
            if !insert && !replace && !is_update {
                continue;
            }
            let mut target = at + 1;
            if (insert || is_update) && word_at(target, "OR") {
                target += 2;
            }
            if insert || replace {
                if !word_at(target, "INTO") {
                    continue;
                }
                target += 1;
            } else if !(target..tokens.len()).any(|index| word_at(index, "SET")) {
                // A display string such as "timing fixture update {id}" is
                // not an UPDATE statement without its SET clause.
                continue;
            }
            if placeholder_at(target) {
                count += 1;
            }
        }
    }
    count
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SqlToken {
    Word(String),
    String(String),
    Symbol(char),
}

/// Tokenize the narrow SQL form the census can prove. Quoted identifiers and
/// comments remain opaque rather than being mistaken for assignment syntax.
pub(super) fn sql_tokens(sql: &str) -> Option<Vec<SqlToken>> {
    let mut tokens = Vec::new();
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '\'' {
            let mut value = String::new();
            loop {
                let next = chars.next()?;
                if next == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        break;
                    }
                }
                value.push(next);
            }
            tokens.push(SqlToken::String(value));
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            let mut word = String::from(ch);
            while chars
                .peek()
                .is_some_and(|next| next.is_ascii_alphanumeric() || *next == '_')
            {
                word.push(chars.next()?);
            }
            tokens.push(SqlToken::Word(word.to_ascii_lowercase()));
        } else if matches!(ch, '"' | '`' | '[' | ']')
            || (ch == '-' && chars.peek() == Some(&'-'))
            || (ch == '/' && chars.peek() == Some(&'*'))
        {
            return None;
        } else {
            tokens.push(SqlToken::Symbol(ch));
        }
    }
    Some(tokens)
}

pub(super) fn sql_word(token: &SqlToken, word: &str) -> bool {
    matches!(token, SqlToken::Word(actual) if actual == word)
}

/// Split arguments or assignments only at their own parenthesis depth.
pub(super) fn sql_parts(tokens: &[SqlToken]) -> Option<Vec<&[SqlToken]>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            SqlToken::Symbol('(') => depth += 1,
            SqlToken::Symbol(')') => depth = depth.checked_sub(1)?,
            SqlToken::Symbol(',') if depth == 0 => {
                parts.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    parts.push(&tokens[start..]);
    parts.iter().all(|part| !part.is_empty()).then_some(parts)
}

/// Only the first argument's transformations can widen the properties write.
/// Path arguments have fixed positions: odd positions after json_set's input,
/// and every argument after json_remove's input. Values and WHERE literals
/// never contribute a key.
pub(super) fn sql_json_key_paths(tokens: &[SqlToken], paths: &mut BTreeSet<String>) -> Option<()> {
    if tokens.len() == 1 && sql_word(&tokens[0], "properties") {
        return Some(());
    }
    let [SqlToken::Word(function), SqlToken::Symbol('('), args @ .., SqlToken::Symbol(')')] =
        tokens
    else {
        return None;
    };
    if !matches!(function.as_str(), "json_set" | "json_remove") {
        return None;
    }
    let args = sql_parts(args)?;
    let (input, changes) = args.split_first()?;
    sql_json_key_paths(input, paths)?;
    let path_args = if function == "json_set" {
        if changes.is_empty() || changes.len() % 2 != 0 {
            return None;
        }
        changes.iter().step_by(2).copied().collect::<Vec<_>>()
    } else {
        if changes.is_empty() {
            return None;
        }
        changes.to_vec()
    };
    for arg in path_args {
        let [SqlToken::String(path)] = arg else {
            return None;
        };
        if !path.starts_with("$.") || !bare_top_level_path(path) {
            return None;
        }
        paths.insert(path.clone());
    }
    Some(())
}

/// Prove the complete key set of one properties assignment in a plain UPDATE.
/// Unknown inputs, dynamic paths and opaque transformations stay whole-object.
pub(super) fn sql_fixed_key_paths(literal: &str) -> Option<BTreeSet<String>> {
    let tokens = sql_tokens(literal)?;
    if tokens.len() < 4
        || tokens.contains(&SqlToken::Symbol(';'))
        || !sql_word(&tokens[0], "update")
        || !(sql_word(&tokens[1], "notes") || sql_word(&tokens[1], "entities"))
        || !sql_word(&tokens[2], "set")
    {
        return None;
    }
    let mut depth = 0usize;
    let mut end = tokens.len();
    for (index, token) in tokens.iter().enumerate().skip(3) {
        match token {
            SqlToken::Symbol('(') => depth += 1,
            SqlToken::Symbol(')') => depth = depth.checked_sub(1)?,
            SqlToken::Word(word) if word == "where" && depth == 0 => {
                end = index;
                break;
            }
            SqlToken::Symbol(';') => return None,
            _ => {}
        }
    }
    let mut paths = None;
    for assignment in sql_parts(&tokens[3..end])? {
        let [SqlToken::Word(column), SqlToken::Symbol('='), expression @ ..] = assignment else {
            return None;
        };
        if column == "properties" {
            if paths.is_some() {
                return None;
            }
            let mut keys = BTreeSet::new();
            sql_json_key_paths(expression, &mut keys)?;
            if keys.is_empty() {
                return None;
            }
            paths = Some(keys);
        }
    }
    paths
}

pub(super) fn sql_single_key_path(literal: &str) -> Option<String> {
    let paths = sql_fixed_key_paths(literal)?;
    (paths.len() == 1)
        .then(|| paths.into_iter().next())
        .flatten()
}

pub(super) fn macro_strings(tokens: TokenStream, strings: &mut Vec<String>) {
    for token in tokens {
        match token {
            TokenTree::Literal(literal) => {
                if let Ok(value) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                    strings.push(value.value());
                }
            }
            TokenTree::Group(group) => macro_strings(group.stream(), strings),
            _ => {}
        }
    }
}
