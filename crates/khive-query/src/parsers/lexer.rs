//! Character cursor shared by the GQL and SPARQL parsers.

use crate::error::QueryError;

pub(super) struct Cursor {
    pub(super) input: Vec<char>,
    pub(super) pos: usize,
}

impl Cursor {
    pub(super) fn new(input: &str) -> Self {
        Self {
            input: input.chars().collect(),
            pos: 0,
        }
    }

    pub(super) fn err(&self, msg: impl Into<String>) -> QueryError {
        QueryError::Parse {
            position: self.pos,
            message: msg.into(),
        }
    }

    pub(super) fn peek(&self) -> Option<char> {
        self.input.get(self.pos).copied()
    }

    pub(super) fn advance(&mut self) -> Option<char> {
        let c = self.input.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    pub(super) fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    pub(super) fn expect_char(&mut self, expected: char) -> Result<(), QueryError> {
        self.skip_whitespace();
        match self.advance() {
            Some(c) if c == expected => Ok(()),
            Some(c) => Err(self.err(format!("expected '{expected}', got '{c}'"))),
            None => Err(self.err(format!("expected '{expected}', got end of input"))),
        }
    }

    pub(super) fn try_keyword(&mut self, kw: &str) -> bool {
        self.skip_whitespace();
        let start = self.pos;
        let kw_upper = kw.to_uppercase();
        for expected_char in kw_upper.chars() {
            match self.advance() {
                Some(c) if c.to_uppercase().next() == Some(expected_char) => {}
                _ => {
                    self.pos = start;
                    return false;
                }
            }
        }
        if let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' {
                self.pos = start;
                return false;
            }
        }
        true
    }

    pub(super) fn expect_keyword(&mut self, kw: &str) -> Result<(), QueryError> {
        if !self.try_keyword(kw) {
            Err(self.err(format!("expected keyword '{kw}'")))
        } else {
            Ok(())
        }
    }

    pub(super) fn parse_ident(&mut self) -> Result<String, QueryError> {
        self.skip_whitespace();
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' {
                self.advance();
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.err("expected identifier"));
        }
        Ok(self.input[start..self.pos].iter().collect())
    }

    pub(super) fn parse_number(&mut self) -> Result<usize, QueryError> {
        self.skip_whitespace();
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.err("expected number"));
        }
        let s: String = self.input[start..self.pos].iter().collect();
        s.parse()
            .map_err(|_| self.err(format!("invalid number: {s}")))
    }

    pub(super) fn parse_string_literal(&mut self) -> Result<String, QueryError> {
        self.skip_whitespace();
        let quote = match self.advance() {
            Some(c @ ('\'' | '"')) => c,
            _ => return Err(self.err("expected string literal")),
        };
        let start = self.pos;
        while let Some(c) = self.advance() {
            if c == quote {
                return Ok(self.input[start..self.pos - 1].iter().collect());
            }
        }
        Err(self.err("unterminated string literal"))
    }

    pub(super) fn parse_limit(&mut self) -> Result<Option<usize>, QueryError> {
        let limit = if self.try_keyword("LIMIT") {
            Some(self.parse_number()?)
        } else {
            None
        };
        Ok(limit)
    }

    pub(super) fn expect_end(&mut self) -> Result<(), QueryError> {
        self.skip_whitespace();
        if self.pos < self.input.len() {
            return Err(self.err(format!(
                "unexpected trailing input: '{}'",
                self.input[self.pos..].iter().collect::<String>()
            )));
        }
        Ok(())
    }
}
