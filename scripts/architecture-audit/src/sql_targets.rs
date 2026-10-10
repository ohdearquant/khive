//! Structural SQL classification for source auditing, without executing SQL.
//!
//! This recognizes table positions, not expression validity or schema existence.
//! Unsupported SQL-looking input retains located diagnostics and any independently
//! recognized targets. Only standalone `{table}` / `{table_name}` relation
//! placeholders have a known dynamic shape; assembled identifiers stay partial.

use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Analysis {
    pub targets: Vec<Target>,
    pub statement: bool,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub relation: String,
    pub role: String,
    pub dynamic: bool,
}

#[derive(Clone, Debug)]
enum Kind {
    Word(String),
    Identifier(String),
    Value,
    Number,
    Placeholder(String),
    Symbol(char),
}

#[derive(Clone, Debug)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

impl Token {
    fn keyword(&self, value: &str) -> bool {
        matches!(&self.kind, Kind::Word(word) if word.eq_ignore_ascii_case(value))
    }

    fn symbol(&self, value: char) -> bool {
        matches!(self.kind, Kind::Symbol(symbol) if symbol == value)
    }
}

fn diagnostic(offset: usize, message: &str) -> String {
    format!("at byte {offset}: {message}")
}

fn template_identifier(value: String) -> Kind {
    let mut rendered = String::new();
    let mut cursor = 0;
    let mut dynamic = false;
    while cursor < value.len() {
        let tail = &value[cursor..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            rendered.push(tail.chars().next().unwrap());
            cursor += 2;
        } else {
            let ch = tail.chars().next().unwrap();
            dynamic |= matches!(ch, '{' | '}');
            rendered.push(ch);
            cursor += ch.len_utf8();
        }
    }
    if dynamic {
        // A mixture of escaped braces and placeholders must not accidentally
        // render into the spelling of a supported whole-name placeholder.
        Kind::Placeholder(value)
    } else {
        Kind::Identifier(rendered)
    }
}

fn lex(sql: &str, template: bool) -> (Vec<Token>, Vec<String>) {
    let mut tokens = Vec::new();
    let mut diagnostics = Vec::new();
    let mut cursor = 0;
    while cursor < sql.len() {
        let tail = &sql[cursor..];
        let ch = tail.chars().next().unwrap();
        if ch.is_whitespace() {
            cursor += ch.len_utf8();
            continue;
        }
        if tail.starts_with("--") {
            cursor += tail.find('\n').unwrap_or(tail.len());
            continue;
        }
        if tail.starts_with("/*") {
            if let Some(end) = tail[2..].find("*/") {
                cursor += end + 4;
            } else {
                diagnostics.push(diagnostic(cursor, "unterminated block comment"));
                break;
            }
            continue;
        }
        let start = cursor;
        let kind = if matches!(ch, '\'' | '"' | '`' | '[') {
            let close = if ch == '[' { ']' } else { ch };
            cursor += ch.len_utf8();
            let mut value = String::new();
            let mut closed = false;
            while cursor < sql.len() {
                let next = sql[cursor..].chars().next().unwrap();
                cursor += next.len_utf8();
                if next == close {
                    if sql[cursor..].starts_with(close) {
                        value.push(close);
                        cursor += close.len_utf8();
                    } else {
                        closed = true;
                        break;
                    }
                } else {
                    value.push(next);
                }
            }
            if !closed {
                diagnostics.push(diagnostic(start, "unterminated quoted token"));
            }
            if ch == '\'' {
                Kind::Value
            } else if template {
                template_identifier(value)
            } else {
                Kind::Identifier(value)
            }
        } else if ch == '{' {
            if template && tail.starts_with("{{") {
                cursor += 2;
                Kind::Symbol('{')
            } else if let Some(end) = tail.find('}') {
                cursor += end + 1;
                Kind::Placeholder(tail[..=end].to_owned())
            } else {
                diagnostics.push(diagnostic(start, "unterminated format placeholder"));
                cursor = sql.len();
                Kind::Placeholder(tail.to_owned())
            }
        } else if template && tail.starts_with("}}") {
            cursor += 2;
            Kind::Symbol('}')
        } else if ch.is_ascii_digit() {
            cursor += ch.len_utf8();
            while let Some(next) = sql[cursor..].chars().next() {
                if next.is_alphanumeric() || matches!(next, '_' | '.') {
                    cursor += next.len_utf8();
                } else {
                    break;
                }
            }
            Kind::Number
        } else if ch.is_alphabetic() || ch == '_' {
            cursor += ch.len_utf8();
            while let Some(next) = sql[cursor..].chars().next() {
                if next.is_alphanumeric() || matches!(next, '_' | '$') {
                    cursor += next.len_utf8();
                } else {
                    break;
                }
            }
            Kind::Word(sql[start..cursor].to_owned())
        } else {
            cursor += ch.len_utf8();
            Kind::Symbol(ch)
        };
        tokens.push(Token {
            kind,
            start,
            end: cursor,
        });
    }
    (tokens, diagnostics)
}

fn sql_start(token: &Token) -> bool {
    concat!(
        "SELECT WITH INSERT UPDATE DELETE REPLACE CREATE ALTER DROP PRAGMA VALUES ",
        "BEGIN COMMIT END ROLLBACK SAVEPOINT RELEASE VACUUM EXPLAIN ATTACH DETACH ANALYZE REINDEX"
    )
    .split_ascii_whitespace()
    .any(|word| token.keyword(word))
}

fn reserved(word: &str) -> bool {
    concat!(
        "SELECT FROM JOIN WHERE SET AS ON USING INTO VALUES DEFAULT RETURNING ORDER GROUP BY ",
        "LIMIT OFFSET HAVING UNION INTERSECT EXCEPT WITH RECURSIVE INSERT UPDATE DELETE REPLACE ",
        "CREATE ALTER DROP TABLE VIEW INDEX TRIGGER VIRTUAL IF NOT EXISTS NULL INNER LEFT RIGHT ",
        "FULL OUTER CROSS NATURAL AND OR WHEN THEN ELSE END BEGIN DISTINCT ALL ADD RENAME TO"
    )
    .split_ascii_whitespace()
    .any(|value| word.eq_ignore_ascii_case(value))
}

// Common lower-case human labels do not become incomplete SQL merely because
// their first word is also a verb. SET/OR and AS/RECURSIVE provide SQL structure;
// uppercase SQL-looking refusals remain diagnostics. This does not validate prose.
fn unanchored_label(tokens: &[Token]) -> bool {
    let Some(Token {
        kind: Kind::Word(first),
        ..
    }) = tokens.first()
    else {
        return false;
    };
    (first == "update"
        && !tokens
            .iter()
            .any(|token| token.keyword("SET") || token.keyword("OR")))
        || (first == "with"
            && !tokens
                .iter()
                .any(|token| token.keyword("AS") || token.keyword("RECURSIVE")))
}

struct Parser<'a> {
    tokens: &'a [Token],
    targets: Vec<(usize, Target)>,
    diagnostics: Vec<String>,
    statement: bool,
    sql_context: bool,
}

impl Parser<'_> {
    fn error(&mut self, index: usize, message: &str) {
        let offset = self.tokens.get(index).map_or_else(
            || self.tokens.last().map_or(0, |token| token.end),
            |token| token.start,
        );
        self.diagnostics.push(diagnostic(offset, message));
    }

    fn kw(&self, index: usize, end: usize, keyword: &str) -> bool {
        index < end && self.tokens[index].keyword(keyword)
    }

    fn matching(&mut self, start: usize, end: usize) -> Option<usize> {
        let mut depth = 0;
        for index in start..end {
            if self.tokens[index].symbol('(') {
                depth += 1;
            } else if self.tokens[index].symbol(')') {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
        }
        self.error(start, "unclosed parenthesized group");
        None
    }

    // Return the decoded spelling, dynamic flag, qualification and next token.
    fn relation(&mut self, start: usize, end: usize) -> Option<(String, bool, bool, usize)> {
        let mut index = start;
        let mut parts = Vec::new();
        let mut dynamic = false;
        loop {
            let Some(token) = self.tokens.get(index).filter(|_| index < end) else {
                self.error(index, "expected a relation identifier");
                return None;
            };
            let part = match &token.kind {
                Kind::Word(value) if !reserved(value) => value.clone(),
                Kind::Identifier(value) if !value.is_empty() => value.clone(),
                Kind::Placeholder(value)
                    if matches!(value.as_str(), "{table}" | "{table_name}") =>
                {
                    dynamic = true;
                    value.clone()
                }
                Kind::Placeholder(_) => {
                    self.error(index, "opaque relation placeholder");
                    return None;
                }
                _ => {
                    self.error(
                        index,
                        "expected a nonnumeric, nonkeyword relation identifier",
                    );
                    return None;
                }
            };
            parts.push(part);
            index += 1;
            if index < end && self.tokens[index].symbol('.') {
                index += 1;
                continue;
            }
            // A format fragment adjoining an identifier is not a complete name.
            if index < end
                && self.tokens[index - 1].end == self.tokens[index].start
                && matches!(
                    &self.tokens[index].kind,
                    Kind::Word(_) | Kind::Identifier(_) | Kind::Placeholder(_)
                )
            {
                self.error(start, "assembled relation identifier is opaque");
                return None;
            }
            break;
        }
        let qualified = parts.len() > 1;
        Some((parts.join("."), dynamic, qualified, index))
    }

    fn target(&mut self, index: usize, name: &str, role: &str, dynamic: bool) {
        self.targets.push((
            self.tokens[index].start,
            Target {
                relation: name.to_owned(),
                role: role.to_owned(),
                dynamic,
            },
        ));
    }

    fn alias_end(&mut self, mut index: usize, end: usize) -> usize {
        if self.kw(index, end, "AS") {
            index += 1;
            if index < end
                && match &self.tokens[index].kind {
                    Kind::Identifier(value) => !value.is_empty(),
                    Kind::Word(value) => !reserved(value),
                    _ => false,
                }
            {
                return index + 1;
            }
            self.error(index, "AS requires an alias identifier");
        } else if index < end {
            match &self.tokens[index].kind {
                Kind::Identifier(_) => return index + 1,
                Kind::Word(word) if !reserved(word) => return index + 1,
                _ => {}
            }
        }
        index
    }

    fn groups(&mut self, start: usize, end: usize, aliases: &BTreeSet<String>, depth: usize) {
        if depth > 64 {
            self.error(start, "SQL nesting exceeds the structural analysis limit");
            return;
        }
        let mut index = start;
        while index < end {
            if self.tokens[index].symbol('(') {
                let Some(close) = self.matching(index, end) else {
                    return;
                };
                if index + 1 < close && sql_start(&self.tokens[index + 1]) {
                    self.statements(index + 1, close, aliases, depth + 1);
                } else {
                    self.groups(index + 1, close, aliases, depth + 1);
                }
                index = close + 1;
            } else {
                index += 1;
            }
        }
    }

    fn sources(
        &mut self,
        start: usize,
        end: usize,
        role: &str,
        aliases: &BTreeSet<String>,
        depth: usize,
    ) -> usize {
        let mut index = start;
        loop {
            if index < end && self.tokens[index].symbol('(') {
                let Some(close) = self.matching(index, end) else {
                    return end;
                };
                if index + 1 < close && sql_start(&self.tokens[index + 1]) {
                    self.statements(index + 1, close, aliases, depth + 1);
                } else {
                    self.error(index, "unsupported parenthesized relation group");
                }
                index = close + 1;
            } else {
                let relation_start = index;
                let Some((name, dynamic, qualified, next)) = self.relation(index, end) else {
                    return end;
                };
                if qualified || dynamic || !aliases.contains(&name.to_ascii_lowercase()) {
                    self.target(relation_start, &name, role, dynamic);
                }
                index = next;
                if index < end && self.tokens[index].symbol('(') {
                    // Table-valued functions have a relation role; their arguments
                    // can themselves contain SQL subqueries.
                    let Some(close) = self.matching(index, end) else {
                        return end;
                    };
                    self.groups(index, close + 1, aliases, depth + 1);
                    index = close + 1;
                }
            }
            index = self.alias_end(index, end);
            if index < end && self.tokens[index].symbol(',') {
                index += 1;
            } else {
                return index;
            }
        }
    }

    fn select(&mut self, start: usize, end: usize, aliases: &BTreeSet<String>, depth: usize) {
        let expression = start + 1;
        if expression == end || self.kw(expression, end, "FROM") {
            self.error(expression, "SELECT requires a result expression");
            return;
        }
        self.statement = true;
        let mut index = expression;
        let mut has_from = false;
        while index < end {
            if self.tokens[index].symbol('(') {
                let Some(close) = self.matching(index, end) else {
                    return;
                };
                self.groups(index, close + 1, aliases, depth + 1);
                index = close + 1;
            } else if self.kw(index, end, "FROM") {
                has_from = true;
                index = self.sources(index + 1, end, "from", aliases, depth);
            } else if self.kw(index, end, "JOIN") {
                if !has_from {
                    self.error(index, "JOIN without a FROM relation");
                    return;
                }
                index = self.sources(index + 1, end, "join", aliases, depth);
            } else {
                index += 1;
            }
        }
    }

    fn with(&mut self, start: usize, end: usize, inherited: &BTreeSet<String>, depth: usize) {
        let mut aliases = inherited.clone();
        let mut bodies = Vec::new();
        let mut index = start + 1;
        if self.kw(index, end, "RECURSIVE") {
            index += 1;
        }
        loop {
            let Some((name, dynamic, qualified, next)) = self.relation(index, end) else {
                return;
            };
            if dynamic || qualified {
                self.error(index, "CTE alias must be a static, unqualified identifier");
                return;
            }
            aliases.insert(name.to_ascii_lowercase());
            index = next;
            if index < end && self.tokens[index].symbol('(') {
                let Some(close) = self.matching(index, end) else {
                    return;
                };
                index = close + 1;
            }
            if !self.kw(index, end, "AS") {
                self.error(index, "CTE requires AS followed by a SQL body");
                return;
            }
            index += 1;
            if self.kw(index, end, "NOT") {
                index += 1;
                if !self.kw(index, end, "MATERIALIZED") {
                    self.error(index, "expected MATERIALIZED after NOT");
                    return;
                }
            }
            if self.kw(index, end, "MATERIALIZED") {
                index += 1;
            }
            if index == end || !self.tokens[index].symbol('(') {
                self.error(index, "CTE requires a parenthesized SQL body");
                return;
            }
            let Some(close) = self.matching(index, end) else {
                return;
            };
            bodies.push((index + 1, close));
            index = close + 1;
            if index < end && self.tokens[index].symbol(',') {
                index += 1;
            } else {
                break;
            }
        }
        if index == end || !sql_start(&self.tokens[index]) {
            self.error(index, "WITH requires a terminal SQL statement");
            return;
        }
        for (body_start, body_end) in bodies {
            if body_start == body_end || !sql_start(&self.tokens[body_start]) {
                self.error(body_start, "CTE body is not a recognized SQL statement");
            } else {
                self.statements(body_start, body_end, &aliases, depth + 1);
            }
        }
        self.one(index, end, &aliases, depth + 1);
    }

    fn conflict_end(&mut self, mut index: usize, end: usize) -> Option<usize> {
        if self.kw(index, end, "OR") {
            index += 1;
            if !["ROLLBACK", "ABORT", "REPLACE", "FAIL", "IGNORE"]
                .iter()
                .any(|value| self.kw(index, end, value))
            {
                self.error(index, "unsupported OR conflict clause");
                return None;
            }
            index += 1;
        }
        Some(index)
    }

    fn insert(&mut self, start: usize, end: usize, aliases: &BTreeSet<String>, depth: usize) {
        let Some(mut index) = self.conflict_end(start + 1, end) else {
            return;
        };
        if !self.kw(index, end, "INTO") {
            self.error(index, "INSERT/REPLACE requires INTO");
            return;
        }
        index += 1;
        let Some((name, dynamic, _, mut next)) = self.relation(index, end) else {
            return;
        };
        next = self.alias_end(next, end);
        if next < end && self.tokens[next].symbol('(') {
            let Some(close) = self.matching(next, end) else {
                return;
            };
            next = close + 1;
        }
        if !["VALUES", "SELECT", "WITH", "DEFAULT"]
            .iter()
            .any(|value| self.kw(next, end, value))
        {
            self.error(
                next,
                "INSERT/REPLACE requires a recognized value or query payload",
            );
            return;
        }
        if (self.kw(next, end, "VALUES") && (next + 1 == end || !self.tokens[next + 1].symbol('(')))
            || (self.kw(next, end, "DEFAULT") && !self.kw(next + 1, end, "VALUES"))
        {
            self.error(next, "expected parenthesized VALUES or DEFAULT VALUES");
            return;
        }
        self.statement = true;
        let role = if self.tokens[start].keyword("REPLACE") {
            "replace"
        } else {
            "insert"
        };
        self.target(index, &name, role, dynamic);
        if self.kw(next, end, "SELECT") || self.kw(next, end, "WITH") {
            self.one(next, end, aliases, depth + 1);
        } else {
            self.groups(next, end, aliases, depth + 1);
        }
    }

    fn update(&mut self, start: usize, end: usize, aliases: &BTreeSet<String>, depth: usize) {
        let Some(index) = self.conflict_end(start + 1, end) else {
            return;
        };
        let Some((name, dynamic, _, next)) = self.relation(index, end) else {
            return;
        };
        let next = self.alias_end(next, end);
        if !self.kw(next, end, "SET") || next + 1 == end {
            self.error(next, "UPDATE requires SET and an assignment");
            return;
        }
        self.statement = true;
        self.target(index, &name, "update", dynamic);
        self.groups(next + 1, end, aliases, depth + 1);
        // SQLite UPDATE ... FROM uses the same relation positions as SELECT.
        let mut cursor = next + 1;
        let mut has_from = false;
        while cursor < end {
            if self.tokens[cursor].symbol('(') {
                let Some(close) = self.matching(cursor, end) else {
                    return;
                };
                cursor = close + 1;
            } else if self.kw(cursor, end, "FROM") {
                has_from = true;
                cursor = self.sources(cursor + 1, end, "from", aliases, depth);
            } else if self.kw(cursor, end, "JOIN") && has_from {
                cursor = self.sources(cursor + 1, end, "join", aliases, depth);
            } else {
                cursor += 1;
            }
        }
    }

    fn exists_end(&mut self, mut index: usize, end: usize, negated: bool) -> Option<usize> {
        if self.kw(index, end, "IF") {
            index += 1;
            if negated {
                if !self.kw(index, end, "NOT") {
                    self.error(index, "expected IF NOT EXISTS");
                    return None;
                }
                index += 1;
            }
            if !self.kw(index, end, "EXISTS") {
                self.error(index, "expected EXISTS");
                return None;
            }
            index += 1;
        }
        Some(index)
    }

    fn ddl(&mut self, start: usize, end: usize, aliases: &BTreeSet<String>, depth: usize) {
        let create = self.tokens[start].keyword("CREATE");
        let alter = self.tokens[start].keyword("ALTER");
        let mut index = start + 1;
        if create && (self.kw(index, end, "TEMP") || self.kw(index, end, "TEMPORARY")) {
            index += 1;
        }
        let unique = create && self.kw(index, end, "UNIQUE");
        if unique {
            index += 1;
        }
        let virtual_table = create && self.kw(index, end, "VIRTUAL");
        if virtual_table {
            index += 1;
        }
        let Some(kind) = ["TABLE", "VIEW", "INDEX", "TRIGGER"]
            .into_iter()
            .find(|value| self.kw(index, end, value))
        else {
            self.error(index, "unsupported DDL object kind");
            return;
        };
        if (unique && kind != "INDEX") || ((virtual_table || alter) && kind != "TABLE") {
            self.error(index, "unsupported DDL modifiers");
            return;
        }
        let Some(index) = self.exists_end(index + 1, end, create) else {
            return;
        };
        let Some((name, dynamic, _, next)) = self.relation(index, end) else {
            return;
        };
        let role = if create {
            if virtual_table {
                "create_virtual_table".to_owned()
            } else {
                format!("create_{}", kind.to_ascii_lowercase())
            }
        } else if alter {
            "alter_table".to_owned()
        } else {
            format!("drop_{}", kind.to_ascii_lowercase())
        };
        if !create && !alter {
            self.statement = true;
            self.target(index, &name, &role, dynamic);
            if next != end {
                self.error(next, "unsupported trailing DROP clause");
            }
            return;
        }
        if alter {
            if !["ADD", "DROP", "RENAME"]
                .iter()
                .any(|value| self.kw(next, end, value))
                || next + 1 == end
            {
                self.error(next, "ALTER TABLE requires ADD, DROP or RENAME content");
                return;
            }
            self.statement = true;
            self.target(index, &name, &role, dynamic);
            return;
        }
        let mut query = None;
        let mut on_relation = None;
        let mut trigger_body = None;
        if kind == "INDEX" || kind == "TRIGGER" {
            let mut cursor = next;
            while cursor < end && !self.kw(cursor, end, "ON") {
                if self.tokens[cursor].symbol('(') {
                    let Some(close) = self.matching(cursor, end) else {
                        return;
                    };
                    cursor = close + 1;
                } else {
                    cursor += 1;
                }
            }
            if cursor == end {
                self.error(next, "CREATE INDEX/TRIGGER requires ON and a relation");
                return;
            }
            let Some((table, table_dynamic, _, tail)) = self.relation(cursor + 1, end) else {
                return;
            };
            if tail == end {
                self.error(tail, "CREATE INDEX/TRIGGER requires a definition after ON");
                return;
            }
            if kind == "INDEX" && !self.tokens[tail].symbol('(') {
                self.error(tail, "CREATE INDEX requires a parenthesized definition");
                return;
            }
            if kind == "TRIGGER" {
                let mut body = tail;
                while body < end && !self.kw(body, end, "BEGIN") {
                    if self.tokens[body].symbol('(') {
                        let Some(close) = self.matching(body, end) else {
                            return;
                        };
                        body = close + 1;
                    } else {
                        body += 1;
                    }
                }
                if body == end || body + 1 == end {
                    self.error(tail, "CREATE TRIGGER requires a BEGIN body");
                    return;
                }
                trigger_body = Some(body + 1);
            }
            on_relation = Some((cursor + 1, table, table_dynamic));
        } else if virtual_table {
            if !self.kw(next, end, "USING") || next + 1 == end {
                self.error(next, "CREATE VIRTUAL TABLE requires USING content");
                return;
            }
        } else if kind == "VIEW" {
            let mut cursor = next;
            if cursor < end && self.tokens[cursor].symbol('(') {
                let Some(close) = self.matching(cursor, end) else {
                    return;
                };
                cursor = close + 1;
            }
            if !self.kw(cursor, end, "AS") || cursor + 1 == end {
                self.error(cursor, "CREATE VIEW requires AS and a query");
                return;
            }
            query = Some(cursor + 1);
        } else if self.kw(next, end, "AS") {
            query = Some(next + 1);
        } else if next == end || !self.tokens[next].symbol('(') {
            self.error(next, "CREATE TABLE requires a definition or AS query");
            return;
        }
        if let Some(query_start) = query {
            if query_start == end
                || !(self.kw(query_start, end, "SELECT") || self.kw(query_start, end, "WITH"))
            {
                self.error(query_start, "CREATE AS requires a recognized query");
                return;
            }
        }
        self.statement = true;
        self.target(index, &name, &role, dynamic);
        if let Some((on_start, table, table_dynamic)) = on_relation {
            self.target(on_start, &table, "on", table_dynamic);
        }
        if let Some(query_start) = query {
            self.one(query_start, end, aliases, depth + 1);
        } else {
            self.groups(next, end, aliases, depth + 1);
            if let Some(body) = trigger_body {
                self.statements(body, end, aliases, depth + 1);
            }
        }
    }

    fn one(&mut self, start: usize, end: usize, aliases: &BTreeSet<String>, depth: usize) {
        if start == end {
            return;
        }
        if unanchored_label(&self.tokens[start..end]) {
            return;
        }
        if depth > 64 {
            self.error(start, "SQL nesting exceeds the structural analysis limit");
            return;
        }
        let first = &self.tokens[start];
        if first.keyword("SELECT") {
            self.select(start, end, aliases, depth);
        } else if first.keyword("WITH") {
            self.with(start, end, aliases, depth);
        } else if first.keyword("INSERT") || first.keyword("REPLACE") {
            self.insert(start, end, aliases, depth);
        } else if first.keyword("UPDATE") {
            self.update(start, end, aliases, depth);
        } else if first.keyword("DELETE") {
            if !self.kw(start + 1, end, "FROM") {
                self.error(start + 1, "DELETE requires FROM");
                return;
            }
            if let Some((name, dynamic, _, next)) = self.relation(start + 2, end) {
                self.statement = true;
                self.target(start + 2, &name, "delete", dynamic);
                self.groups(next, end, aliases, depth + 1);
            }
        } else if first.keyword("CREATE") || first.keyword("ALTER") || first.keyword("DROP") {
            self.ddl(start, end, aliases, depth);
        } else if first.keyword("PRAGMA") {
            if let Some((_, dynamic, _, _)) = self.relation(start + 1, end) {
                if dynamic {
                    self.error(start + 1, "opaque PRAGMA name");
                    return;
                }
                self.statement = true;
            }
        } else if first.keyword("VALUES") {
            if start + 1 == end || !self.tokens[start + 1].symbol('(') {
                self.error(start, "VALUES requires parenthesized values");
            } else {
                self.statement = true;
                self.groups(start + 1, end, aliases, depth + 1);
            }
        } else if [
            "BEGIN",
            "COMMIT",
            "END",
            "ROLLBACK",
            "SAVEPOINT",
            "RELEASE",
            "VACUUM",
        ]
        .iter()
        .any(|value| first.keyword(value))
        {
            self.statement = true;
        } else if sql_start(first) {
            self.error(start, "unsupported SQL statement family");
        } else if matches!(&first.kind, Kind::Placeholder(_))
            && (self.sql_context
                || self.tokens[start + 1..end].iter().any(|token| {
                    sql_start(token)
                        || token.keyword("FROM")
                        || token.keyword("INTO")
                        || token.keyword("SET")
                }))
        {
            self.error(start, "opaque assembled SQL fragment");
        }
    }

    fn statements(&mut self, start: usize, end: usize, aliases: &BTreeSet<String>, depth: usize) {
        let mut part = start;
        let mut nesting = 0;
        for index in start..end {
            if self.tokens[index].symbol('(') {
                nesting += 1;
            } else if self.tokens[index].symbol(')') {
                nesting -= 1;
            } else if nesting == 0 && self.tokens[index].symbol(';') {
                self.one(part, index, aliases, depth);
                part = index + 1;
            }
        }
        self.one(part, end, aliases, depth);
    }
}

/// Analyze a decoded SQL literal. Quoted brace-bearing names are literal names.
pub(crate) fn analyze(sql: &str) -> Analysis {
    analyze_inner(sql, false)
}

/// Analyze a decoded Rust format template, before its arguments are substituted.
/// Doubled braces are literals; only known whole relation placeholders are dynamic.
pub(crate) fn analyze_template(sql: &str) -> Analysis {
    analyze_inner(sql, true)
}

fn analyze_inner(sql: &str, template: bool) -> Analysis {
    let (tokens, lexical_diagnostics) = lex(sql, template);
    let first = tokens.iter().position(|token| !token.symbol(';'));
    let relevant = tokens.iter().any(sql_start)
        && first.is_some_and(|index| {
            !unanchored_label(&tokens[index..])
                && (sql_start(&tokens[index])
                    || matches!(&tokens[index].kind, Kind::Placeholder(_)))
        });
    let mut parser = Parser {
        tokens: &tokens,
        targets: Vec::new(),
        diagnostics: if relevant {
            lexical_diagnostics
        } else {
            Vec::new()
        },
        statement: false,
        sql_context: relevant,
    };
    if relevant {
        let mut opens = Vec::new();
        for (index, token) in tokens.iter().enumerate() {
            if token.symbol('(') {
                opens.push(index);
            } else if token.symbol(')') && opens.pop().is_none() {
                parser.error(index, "unmatched closing parenthesis");
            }
        }
        for index in opens {
            parser.error(index, "unclosed parenthesized group");
        }
    }
    parser.statements(0, tokens.len(), &BTreeSet::new(), 0);
    parser.targets.sort_by_key(|(offset, _)| *offset);
    parser.targets.dedup_by(|left, right| left == right);
    parser.diagnostics.sort();
    parser.diagnostics.dedup();
    Analysis {
        targets: parser
            .targets
            .into_iter()
            .map(|(_, target)| target)
            .collect(),
        statement: parser.statement,
        diagnostics: parser.diagnostics,
    }
}

#[cfg(test)]
#[path = "sql_targets_controls.rs"]
mod controls;
