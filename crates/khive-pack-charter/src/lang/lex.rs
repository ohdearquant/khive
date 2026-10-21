//! Line lexer for charter and vocabulary sources.
//!
//! Sources are indentation-structured. Each non-blank line becomes one [`Line`] carrying its
//! indent width and tokens; `#` starts a comment outside a string literal. Tabs in indentation
//! are refused so that two editors cannot disagree about block structure.

use super::Diagnostic;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Word(String),
    Str(String),
    Int(i64),
    Duration(i64),
    LParen,
    RParen,
    Comma,
    Eq,
    Colon,
    /// Any operator-shaped run (`==`, `>=`, `&&`, `!`, `+`, ...). The language has no operators;
    /// the token exists so the checker can name the construct it refuses.
    Op(String),
}

#[derive(Clone, Debug)]
pub struct Line {
    pub number: usize,
    pub indent: usize,
    pub toks: Vec<Tok>,
}

pub fn lex(source: &str) -> Result<Vec<Line>, Vec<Diagnostic>> {
    let mut lines = Vec::new();
    let mut errors = Vec::new();
    for (index, raw) in source.lines().enumerate() {
        let number = index + 1;
        let leading = &raw[..raw.len() - raw.trim_start().len()];
        if leading.contains('\t') {
            errors.push(Diagnostic::new(
                number,
                "indent_tab",
                "indentation uses a tab; indent with spaces",
            ));
            continue;
        }
        let indent = leading.len();
        match lex_line(raw.trim_start(), number) {
            Ok(toks) if toks.is_empty() => {}
            Ok(toks) => lines.push(Line {
                number,
                indent,
                toks,
            }),
            Err(error) => errors.push(error),
        }
    }
    if errors.is_empty() {
        Ok(lines)
    } else {
        Err(errors)
    }
}

fn lex_line(text: &str, number: usize) -> Result<Vec<Tok>, Diagnostic> {
    let chars: Vec<char> = text.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' => i += 1,
            '#' => break,
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            ',' => {
                toks.push(Tok::Comma);
                i += 1;
            }
            ':' => {
                toks.push(Tok::Colon);
                i += 1;
            }
            '"' => {
                let mut value = String::new();
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    match chars[i] {
                        '"' => {
                            closed = true;
                            i += 1;
                            break;
                        }
                        '\\' if i + 1 < chars.len() => {
                            value.push(chars[i + 1]);
                            i += 2;
                        }
                        other => {
                            value.push(other);
                            i += 1;
                        }
                    }
                }
                if !closed {
                    return Err(Diagnostic::new(
                        number,
                        "unterminated_string",
                        "string literal is not closed",
                    ));
                }
                toks.push(Tok::Str(value));
            }
            '0'..='9' => {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let digits: String = chars[start..i].iter().collect();
                let value: i64 = digits.parse().map_err(|_| {
                    Diagnostic::new(number, "integer_range", "integer literal is out of range")
                })?;
                let unit = chars.get(i).copied();
                let next_is_word = chars
                    .get(i + 1)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_');
                match unit {
                    Some(u @ ('s' | 'm' | 'h' | 'd')) if !next_is_word => {
                        let scale = match u {
                            's' => 1,
                            'm' => 60,
                            'h' => 3_600,
                            _ => 86_400,
                        };
                        toks.push(Tok::Duration(value.saturating_mul(scale)));
                        i += 1;
                    }
                    Some(u) if u.is_ascii_alphabetic() || u == '_' => {
                        return Err(Diagnostic::new(
                            number,
                            "bad_literal",
                            "a number is followed by letters; durations take one of s, m, h, d",
                        ));
                    }
                    _ => toks.push(Tok::Int(value)),
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric()
                        || chars[i] == '_'
                        || chars[i] == '.'
                        || chars[i] == '-')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                toks.push(Tok::Word(word));
            }
            '=' if chars.get(i + 1) != Some(&'=') => {
                toks.push(Tok::Eq);
                i += 1;
            }
            _ => {
                let start = i;
                while i < chars.len() && "=!<>&|+-*/%^~".contains(chars[i]) {
                    i += 1;
                }
                if i == start {
                    return Err(Diagnostic::new(
                        number,
                        "bad_character",
                        format!("unexpected character {c:?}"),
                    ));
                }
                toks.push(Tok::Op(chars[start..i].iter().collect()));
            }
        }
    }
    Ok(toks)
}

/// A line and the lines indented under it.
#[derive(Clone, Debug)]
pub struct Node {
    pub line: Line,
    pub children: Vec<Node>,
}

/// Build the indentation tree. A child block is every following line indented deeper than its
/// parent; a dedent must return to an indent already in use.
pub fn tree(lines: Vec<Line>) -> Result<Vec<Node>, Vec<Diagnostic>> {
    let mut iter = lines.into_iter().peekable();
    let mut errors = Vec::new();
    let nodes = block(&mut iter, None, &mut errors);
    if errors.is_empty() {
        Ok(nodes)
    } else {
        Err(errors)
    }
}

fn block(
    iter: &mut std::iter::Peekable<std::vec::IntoIter<Line>>,
    parent_indent: Option<usize>,
    errors: &mut Vec<Diagnostic>,
) -> Vec<Node> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut level: Option<usize> = None;
    while let Some(next) = iter.peek() {
        if parent_indent.is_some_and(|p| next.indent <= p) {
            break;
        }
        match level {
            None => level = Some(next.indent),
            Some(l) if next.indent < l => {
                errors.push(Diagnostic::new(
                    next.number,
                    "indent_mismatch",
                    "this line dedents to a level no enclosing block uses",
                ));
                iter.next();
                continue;
            }
            Some(l) if next.indent > l => {
                let deeper = next.number;
                let children = block(iter, Some(l), errors);
                match nodes.last_mut() {
                    Some(last) if last.children.is_empty() => last.children = children,
                    _ => errors.push(Diagnostic::new(
                        deeper,
                        "indent_unexpected",
                        "unexpected indent",
                    )),
                }
                continue;
            }
            Some(_) => {}
        }
        let line = iter.next().expect("peeked");
        nodes.push(Node {
            line,
            children: Vec::new(),
        });
    }
    nodes
}
