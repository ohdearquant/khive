//! Parser from the indentation tree to [`super::ast`] forms. No registry is consulted here.

use super::ast::*;
use super::lex::{Node, Tok};
use super::Diagnostic;

const EXPRESSION_REFUSAL: &str = "operators are not part of the charter language: a phrase \
     reports an outcome label, `require` names the phrase, and `when <phrase> is <label>` routes \
     on its outcome";

const PHASE_KEYWORDS: &str = "await, require, action, perform, when, evidence, grant, grants, \
     max_rounds, goto, complete, terminate";

struct Cursor<'a> {
    line: usize,
    toks: &'a [Tok],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(node: &'a Node) -> Self {
        Self {
            line: node.line.number,
            toks: &node.line.toks,
            pos: 0,
        }
    }

    fn peek(&self) -> Option<&'a Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<&'a Tok> {
        let tok = self.toks.get(self.pos);
        self.pos += 1;
        tok
    }

    fn done(&self) -> bool {
        self.pos >= self.toks.len()
    }

    fn err(&self, code: &'static str, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(self.line, code, message)
    }

    fn word(&mut self, what: &str) -> Result<&'a str, Diagnostic> {
        match self.next() {
            Some(Tok::Word(w)) => Ok(w),
            Some(Tok::Op(_)) => Err(self.err("expression", EXPRESSION_REFUSAL)),
            Some(other) => Err(self.err(
                "expected_word",
                format!("expected {what}, found {}", describe(other)),
            )),
            None => Err(self.err("expected_word", format!("expected {what}"))),
        }
    }

    fn ident(&mut self, what: &str) -> Result<String, Diagnostic> {
        let word = self.word(what)?;
        if !is_ident(word) {
            return Err(self.err(
                "bad_identifier",
                format!("{what} {word:?} must be lowercase letters, digits and underscores"),
            ));
        }
        Ok(word.to_owned())
    }

    fn dotted(&mut self, what: &str) -> Result<String, Diagnostic> {
        let word = self.word(what)?;
        if !is_dotted(word) {
            return Err(self.err(
                "bad_identifier",
                format!(
                    "{what} {word:?} must be lowercase segments of letters, digits and \
                     underscores joined by dots"
                ),
            ));
        }
        Ok(word.to_owned())
    }

    fn keyword(&mut self, expected: &str) -> Result<(), Diagnostic> {
        match self.next() {
            Some(Tok::Word(w)) if w == expected => Ok(()),
            Some(Tok::Op(_)) => Err(self.err("expression", EXPRESSION_REFUSAL)),
            Some(other) => Err(self.err(
                "expected_keyword",
                format!("expected `{expected}`, found {}", describe(other)),
            )),
            None => Err(self.err("expected_keyword", format!("expected `{expected}`"))),
        }
    }

    fn opt_string(&mut self) -> Option<String> {
        match self.peek() {
            Some(Tok::Str(s)) => {
                self.pos += 1;
                Some(s.clone())
            }
            _ => None,
        }
    }

    fn colon(&mut self) -> Result<(), Diagnostic> {
        match self.next() {
            Some(Tok::Colon) => Ok(()),
            _ => Err(self.err("expected_colon", "expected `:` to open a block")),
        }
    }

    fn end(&self) -> Result<(), Diagnostic> {
        match self.peek() {
            None => Ok(()),
            Some(Tok::Op(_)) => Err(self.err("expression", EXPRESSION_REFUSAL)),
            Some(other) => Err(self.err(
                "trailing_tokens",
                format!("unexpected {} at the end of the line", describe(other)),
            )),
        }
    }

    fn version(&mut self) -> Result<i64, Diagnostic> {
        let word = self.word("a version such as v1")?;
        word.strip_prefix('v')
            .and_then(|n| n.parse::<i64>().ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                self.err(
                    "bad_version",
                    format!("version {word:?} must be v followed by a positive integer, e.g. v1"),
                )
            })
    }

    fn call(&mut self, what: &str) -> Result<Call, Diagnostic> {
        let line = self.line;
        let name = self.dotted(what)?;
        let mut args = Vec::new();
        if matches!(self.peek(), Some(Tok::LParen)) {
            self.pos += 1;
            if matches!(self.peek(), Some(Tok::RParen)) {
                self.pos += 1;
                return Ok(Call { line, name, args });
            }
            loop {
                let arg_name = match (self.peek(), self.toks.get(self.pos + 1)) {
                    (Some(Tok::Word(w)), Some(Tok::Eq)) => {
                        self.pos += 2;
                        if !is_ident(w) {
                            return Err(self.err(
                                "bad_identifier",
                                format!("argument name {w:?} must be lowercase"),
                            ));
                        }
                        Some(w.clone())
                    }
                    _ => None,
                };
                let value = self.literal()?;
                args.push(Arg {
                    name: arg_name,
                    value,
                });
                match self.next() {
                    Some(Tok::Comma) => continue,
                    Some(Tok::RParen) => break,
                    Some(Tok::Op(_)) => return Err(self.err("expression", EXPRESSION_REFUSAL)),
                    _ => {
                        return Err(self.err(
                            "bad_arguments",
                            "arguments are `name=value` separated by commas inside parentheses",
                        ))
                    }
                }
            }
        }
        Ok(Call { line, name, args })
    }

    fn literal(&mut self) -> Result<Literal, Diagnostic> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(Literal::String(s.clone())),
            Some(Tok::Int(n)) => Ok(Literal::Integer(*n)),
            Some(Tok::Duration(n)) => Ok(Literal::Duration(*n)),
            Some(Tok::Word(w)) if w == "true" => Ok(Literal::Bool(true)),
            Some(Tok::Word(w)) if w == "false" => Ok(Literal::Bool(false)),
            Some(Tok::Word(w)) if w.contains('.') => Err(self.err(
                "reference",
                format!(
                    "{w:?} reads like a reference; arguments are literals only, and the run's \
                     subject and its current generation are implicit"
                ),
            )),
            Some(Tok::Word(w)) if is_ident(w) => Ok(Literal::Word(w.clone())),
            Some(Tok::Op(_)) => Err(self.err("expression", EXPRESSION_REFUSAL)),
            other => Err(self.err(
                "bad_literal",
                format!(
                    "expected a literal (string, integer, duration, true, false or a word), \
                     found {}",
                    other
                        .map(describe)
                        .unwrap_or_else(|| "the end of the line".into())
                ),
            )),
        }
    }

    fn end_clause(&mut self) -> Result<End, Diagnostic> {
        let word = self.word("goto, complete or terminate")?;
        match word {
            "goto" => Ok(End::Goto(self.ident("a phase id")?)),
            "complete" => Ok(End::Complete),
            "terminate" => Ok(End::Terminate(self.opt_string())),
            other => Err(self.err(
                "bad_end",
                format!("expected goto <phase>, complete or terminate, found {other:?}"),
            )),
        }
    }
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Word(w) => format!("{w:?}"),
        Tok::Str(s) => format!("string {s:?}"),
        Tok::Int(n) => format!("integer {n}"),
        Tok::Duration(n) => format!("duration {n}s"),
        Tok::LParen => "`(`".into(),
        Tok::RParen => "`)`".into(),
        Tok::Comma => "`,`".into(),
        Tok::Eq => "`=`".into(),
        Tok::Colon => "`:`".into(),
        Tok::Op(o) => format!("operator {o:?}"),
    }
}

pub fn is_ident(word: &str) -> bool {
    let mut chars = word.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn is_dotted(word: &str) -> bool {
    !word.is_empty() && word.split('.').all(is_ident)
}

fn first_word(node: &Node) -> Option<&str> {
    match node.line.toks.first() {
        Some(Tok::Word(w)) => Some(w),
        _ => None,
    }
}

fn no_children(node: &Node, errors: &mut Vec<Diagnostic>) {
    if let Some(child) = node.children.first() {
        errors.push(Diagnostic::new(
            child.line.number,
            "unexpected_block",
            "this line cannot open a block",
        ));
    }
}

fn line_has_operator(node: &Node) -> bool {
    node.line.toks.iter().any(|t| matches!(t, Tok::Op(_)))
}

pub fn parse_charter(nodes: &[Node]) -> Result<CharterSource, Vec<Diagnostic>> {
    let mut errors = Vec::new();
    let Some(head) = nodes.first() else {
        return Err(vec![Diagnostic::new(
            1,
            "empty",
            "a charter source starts with `charter <id> v<version>`",
        )]);
    };
    let (id, version) = match parse_header(head, "charter") {
        Ok(v) => v,
        Err(e) => return Err(vec![e]),
    };
    no_children(head, &mut errors);
    let mut charter = CharterSource {
        id,
        version,
        title: None,
        subject: None,
        roles: Vec::new(),
        triggers: Vec::new(),
        phases: Vec::new(),
        break_glass: None,
    };
    let mut saw_workflow = false;
    for node in &nodes[1..] {
        let mut c = Cursor::new(node);
        let result: Result<(), Diagnostic> = (|| {
            match c.word("a section keyword")? {
                "title" => {
                    charter.title =
                        Some(c.opt_string().ok_or_else(|| {
                            c.err("expected_string", "title takes a quoted string")
                        })?);
                    c.end()?;
                    no_children(node, &mut errors);
                }
                "subject" => {
                    charter.subject = Some((c.line, c.ident("a subject kind")?));
                    c.end()?;
                    no_children(node, &mut errors);
                }
                "roles" => {
                    c.colon()?;
                    c.end()?;
                    for child in &node.children {
                        let mut r = Cursor::new(child);
                        match (|| {
                            let role = r.ident("a role name")?;
                            let desc = r.opt_string();
                            r.end()?;
                            Ok((r.line, role, desc))
                        })() {
                            Ok(role) => charter.roles.push(role),
                            Err(e) => errors.push(e),
                        }
                        no_children(child, &mut errors);
                    }
                }
                "triggers" => {
                    c.colon()?;
                    c.end()?;
                    for child in &node.children {
                        let mut t = Cursor::new(child);
                        match (|| {
                            let name = t.dotted("a trigger kind")?;
                            t.opt_string();
                            t.end()?;
                            Ok((t.line, name))
                        })() {
                            Ok(trigger) => charter.triggers.push(trigger),
                            Err(e) => errors.push(e),
                        }
                        no_children(child, &mut errors);
                    }
                }
                "workflow" => {
                    if saw_workflow {
                        return Err(c.err("duplicate_section", "a charter has one workflow"));
                    }
                    saw_workflow = true;
                    if matches!(c.peek(), Some(Tok::Word(_))) {
                        c.ident("a workflow name")?;
                    }
                    c.colon()?;
                    c.end()?;
                    for child in &node.children {
                        match parse_phase(child, &mut errors) {
                            Ok(phase) => charter.phases.push(phase),
                            Err(e) => errors.push(e),
                        }
                    }
                }
                "break_glass" => {
                    c.colon()?;
                    c.end()?;
                    charter.break_glass = Some(parse_break_glass(node, &mut errors));
                }
                other => {
                    return Err(c.err(
                        "unknown_section",
                        format!(
                            "unknown section {other:?}; sections are title, subject, roles, \
                             triggers, workflow and break_glass"
                        ),
                    ))
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            errors.push(e);
        }
    }
    if !saw_workflow {
        errors.push(Diagnostic::new(
            head.line.number,
            "missing_workflow",
            "a charter declares a `workflow:` block",
        ));
    }
    if errors.is_empty() {
        Ok(charter)
    } else {
        Err(errors)
    }
}

fn parse_header(node: &Node, keyword: &str) -> Result<(String, i64), Diagnostic> {
    let mut c = Cursor::new(node);
    c.keyword(keyword).map_err(|_| {
        c.err(
            "missing_header",
            format!("the source starts with `{keyword} <id> v<version>`"),
        )
    })?;
    let id = if keyword == "vocabulary" {
        c.dotted("a vocabulary name")?
    } else {
        c.ident("a charter id")?
    };
    let version = c.version()?;
    c.end()?;
    Ok((id, version))
}

fn parse_phase(node: &Node, errors: &mut Vec<Diagnostic>) -> Result<Phase, Diagnostic> {
    let mut c = Cursor::new(node);
    c.keyword("phase")?;
    let id = c.ident("a phase id")?;
    let owner = if matches!(c.peek(), Some(Tok::Word(w)) if w == "by") {
        c.pos += 1;
        Some(c.ident("a role name")?)
    } else {
        None
    };
    c.colon()?;
    c.end()?;
    let mut phase = Phase {
        line: node.line.number,
        id,
        owner,
        awaits: Vec::new(),
        actions: Vec::new(),
        performs: Vec::new(),
        requires: Vec::new(),
        whens: Vec::new(),
        evidence: Vec::new(),
        grants: Vec::new(),
        max_rounds: None,
        end: None,
    };
    if node.children.is_empty() {
        errors.push(Diagnostic::new(
            node.line.number,
            "empty_phase",
            "a phase needs at least one step",
        ));
    }
    for step in &node.children {
        if let Some((line, _)) = &phase.end {
            errors.push(Diagnostic::new(
                step.line.number,
                "after_end",
                format!("this step follows the phase's end clause on line {line}"),
            ));
            break;
        }
        if let Err(e) = parse_step(step, &mut phase, errors) {
            errors.push(e);
        }
    }
    Ok(phase)
}

fn parse_step(
    node: &Node,
    phase: &mut Phase,
    errors: &mut Vec<Diagnostic>,
) -> Result<(), Diagnostic> {
    let mut c = Cursor::new(node);
    let keyword = c.word("a phase step")?;
    if keyword != "grants" {
        no_children(node, errors);
    }
    match keyword {
        "await" => {
            let mut triggers = vec![c.dotted("a trigger kind")?];
            while matches!(c.peek(), Some(Tok::Word(w)) if w == "or") {
                c.pos += 1;
                triggers.push(c.dotted("a trigger kind")?);
            }
            c.end()?;
            phase.awaits.push((c.line, triggers));
        }
        "require" => {
            let call = c.call("a phrase")?;
            let level_word = c.word("a level: hard, soft or advisory")?;
            let level = Level::parse(level_word).ok_or_else(|| {
                c.err(
                    "bad_level",
                    format!("level {level_word:?} must be hard, soft or advisory"),
                )
            })?;
            c.end()?;
            phase.requires.push(Require { call, level });
        }
        "action" => {
            let call = c.call("an action")?;
            let by = if matches!(c.peek(), Some(Tok::Word(w)) if w == "by") {
                c.pos += 1;
                Some(c.ident("a role name")?)
            } else {
                None
            };
            c.end()?;
            phase.actions.push(ActionStep { call, by });
        }
        "when" => {
            let call = c.call("a phrase")?;
            c.keyword("is")?;
            let label = c.ident("an outcome label")?;
            c.colon()?;
            let end = c.end_clause()?;
            c.end()?;
            phase.whens.push(When {
                line: c.line,
                call,
                label,
                end,
            });
        }
        "perform" => {
            let charter = c.ident("a charter id")?;
            c.end()?;
            phase.performs.push((c.line, charter));
        }
        "evidence" => {
            phase.evidence.push(c.ident("an evidence name")?);
            c.end()?;
        }
        "grant" => {
            let role = c.ident("a role name")?;
            c.keyword("may")?;
            let action = c.dotted("an action")?;
            if matches!(c.peek(), Some(Tok::LParen)) {
                return Err(c.err(
                    "bad_grant",
                    "a grant names an action, not a call; the subject is implicit",
                ));
            }
            c.end()?;
            phase.grants.push(Grant {
                line: c.line,
                role,
                action,
            });
        }
        "grants" => {
            if matches!(c.peek(), Some(Tok::Word(w)) if w == "for") {
                c.pos += 1;
                let named = c.ident("a phase id")?;
                if named != phase.id {
                    return Err(c.err(
                        "bad_grant",
                        format!(
                            "grants inside phase {:?} are scoped to it; `grants for {named}` \
                             belongs in that phase",
                            phase.id
                        ),
                    ));
                }
            }
            c.colon()?;
            c.end()?;
            for child in &node.children {
                let mut g = Cursor::new(child);
                match (|| {
                    let role = g.ident("a role name")?;
                    g.keyword("may")?;
                    let action = g.dotted("an action")?;
                    if matches!(g.peek(), Some(Tok::LParen)) {
                        return Err(g.err(
                            "bad_grant",
                            "a grant names an action, not a call; the subject is implicit",
                        ));
                    }
                    g.end()?;
                    Ok(Grant {
                        line: g.line,
                        role,
                        action,
                    })
                })() {
                    Ok(grant) => phase.grants.push(grant),
                    Err(e) => errors.push(e),
                }
            }
        }
        "max_rounds" => {
            let n = match c.next() {
                Some(Tok::Int(n)) if *n > 0 && *n <= 1_000 => *n as u32,
                _ => {
                    return Err(c.err(
                        "bad_max_rounds",
                        "max_rounds takes an integer from 1 to 1000",
                    ))
                }
            };
            c.keyword("then")?;
            let target = c.ident("a phase id")?;
            c.end()?;
            if phase.max_rounds.is_some() {
                return Err(c.err("duplicate_step", "a phase has one max_rounds"));
            }
            phase.max_rounds = Some((c.line, n, target));
        }
        "goto" | "complete" | "terminate" => {
            c.pos = 0;
            let end = c.end_clause()?;
            c.end()?;
            phase.end = Some((c.line, end));
        }
        other => {
            if line_has_operator(node) {
                return Err(c.err("expression", EXPRESSION_REFUSAL));
            }
            return Err(c.err(
                "unknown_step",
                format!("unknown phase step {other:?}; steps are {PHASE_KEYWORDS}"),
            ));
        }
    }
    Ok(())
}

fn parse_break_glass(node: &Node, errors: &mut Vec<Diagnostic>) -> BreakGlass {
    let mut bg = BreakGlass {
        line: node.line.number,
        by: None,
        on: Vec::new(),
        reason_min: None,
        marks_degraded: false,
    };
    for child in &node.children {
        no_children(child, errors);
        let mut c = Cursor::new(child);
        let result: Result<(), Diagnostic> = (|| {
            match c.word("a break_glass field")? {
                "by" => bg.by = Some(c.ident("a role name")?),
                "on" => {
                    let target = c.dotted("<phase>.<phrase>")?;
                    if !target.contains('.') {
                        return Err(c.err(
                            "bad_break_glass",
                            "`on` names <phase>.<phrase>, the requirement it may override",
                        ));
                    }
                    bg.on.push(target);
                }
                "reason_min" => match c.next() {
                    Some(Tok::Int(n)) if *n > 0 => bg.reason_min = Some(*n),
                    _ => {
                        return Err(c.err("bad_break_glass", "reason_min takes a positive integer"))
                    }
                },
                "marks_degraded" => bg.marks_degraded = true,
                other => {
                    if line_has_operator(child) {
                        return Err(c.err("expression", EXPRESSION_REFUSAL));
                    }
                    return Err(c.err(
                        "bad_break_glass",
                        format!(
                            "unknown break_glass field {other:?}; fields are by, on, reason_min, \
                             marks_degraded"
                        ),
                    ));
                }
            }
            c.end()
        })();
        if let Err(e) = result {
            errors.push(e);
        }
    }
    bg
}

pub fn parse_vocabulary(nodes: &[Node]) -> Result<VocabularySource, Vec<Diagnostic>> {
    let mut errors = Vec::new();
    let Some(head) = nodes.first() else {
        return Err(vec![Diagnostic::new(
            1,
            "empty",
            "a vocabulary source starts with `vocabulary <name> v<version>`",
        )]);
    };
    let (name, version) = match parse_header(head, "vocabulary") {
        Ok(v) => v,
        Err(e) => return Err(vec![e]),
    };
    no_children(head, &mut errors);
    let mut entries = Vec::new();
    for node in &nodes[1..] {
        match parse_entry(node, &mut errors) {
            Ok(entry) => entries.push(entry),
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(VocabularySource {
            name,
            version,
            entries,
        })
    } else {
        Err(errors)
    }
}

fn parse_entry(node: &Node, errors: &mut Vec<Diagnostic>) -> Result<EntrySource, Diagnostic> {
    let mut c = Cursor::new(node);
    let kind = match c.word("subject, trigger, phrase or action")? {
        "subject" => EntryKind::Subject,
        "trigger" => EntryKind::Trigger,
        "phrase" => EntryKind::Phrase,
        "action" => EntryKind::Action,
        other => {
            return Err(c.err(
                "unknown_entry",
                format!("unknown entry {other:?}; entries are subject, trigger, phrase, action"),
            ))
        }
    };
    let name = if kind == EntryKind::Subject {
        c.ident("a subject kind")?
    } else {
        c.dotted("a name")?
    };
    let description = c.opt_string();
    let opens_block = matches!(c.peek(), Some(Tok::Colon));
    if opens_block {
        c.pos += 1;
    }
    c.end()?;
    if !opens_block {
        no_children(node, errors);
    }
    let mut entry = EntrySource {
        line: node.line.number,
        kind,
        name,
        description,
        outcomes: None,
        passes: None,
        ceiling: None,
        params: Vec::new(),
    };
    for child in &node.children {
        no_children(child, errors);
        let mut f = Cursor::new(child);
        let result: Result<(), Diagnostic> = (|| {
            match f.word("an entry field")? {
                "outcomes" => {
                    let mut labels = Vec::new();
                    while !f.done() {
                        labels.push(f.ident("an outcome label")?);
                    }
                    entry.outcomes = Some(labels);
                }
                "passes" => {
                    let mut labels = Vec::new();
                    while !f.done() {
                        labels.push(f.ident("an outcome label")?);
                    }
                    entry.passes = Some(labels);
                }
                "ceiling" => {
                    let word = f.word("hard, soft or advisory")?;
                    entry.ceiling = Some(Level::parse(word).ok_or_else(|| {
                        f.err(
                            "bad_level",
                            format!("ceiling {word:?} must be hard, soft or advisory"),
                        )
                    })?);
                    f.end()?;
                }
                "param" => {
                    let name = f.ident("a parameter name")?;
                    let ty = f.ident("a parameter type")?;
                    if !matches!(
                        ty.as_str(),
                        "string" | "integer" | "duration" | "bool" | "word"
                    ) {
                        return Err(f.err(
                            "bad_param",
                            format!(
                                "parameter type {ty:?} must be string, integer, duration, bool \
                                 or word"
                            ),
                        ));
                    }
                    let optional = matches!(f.peek(), Some(Tok::Word(w)) if w == "optional");
                    if optional {
                        f.pos += 1;
                    }
                    f.end()?;
                    entry.params.push(ParamSource { name, ty, optional });
                }
                other => {
                    return Err(f.err(
                        "unknown_field",
                        format!(
                            "unknown entry field {other:?}; fields are outcomes, passes, ceiling, \
                             param"
                        ),
                    ))
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            errors.push(e);
        }
    }
    Ok(entry)
}
