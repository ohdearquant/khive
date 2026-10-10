use std::io::BufRead;

use crate::AdapterError;

use super::{limit_error, Limits};

pub(super) struct Frame {
    pub source: String,
    pub line: usize,
    pub offset: u64,
    pub regular: bool,
    pub complete: bool,
}

pub(super) struct BibtexFrames<R> {
    reader: R,
    limits: Limits,
    offset: u64,
    line: usize,
    utf8_remaining: u8,
    utf8_lower: u8,
    utf8_upper: u8,
}

impl<R: BufRead> BibtexFrames<R> {
    pub fn new(reader: R, limits: Limits) -> Self {
        Self {
            reader,
            limits,
            offset: 0,
            line: 1,
            utf8_remaining: 0,
            utf8_lower: 0x80,
            utf8_upper: 0xbf,
        }
    }

    fn take(&mut self) -> Result<Option<u8>, AdapterError> {
        let next = self
            .reader
            .fill_buf()
            .map_err(|error| AdapterError::Parse(format!("BibTeX source IO: {error}")))?
            .first()
            .copied();
        let Some(byte) = next else {
            if self.utf8_remaining != 0 {
                return Err(AdapterError::Parse("BibTeX source is not UTF-8".into()));
            }
            return Ok(None);
        };
        self.reader.consume(1);
        if self.utf8_remaining != 0 {
            if !(self.utf8_lower..=self.utf8_upper).contains(&byte) {
                return Err(AdapterError::Parse("BibTeX source is not UTF-8".into()));
            }
            self.utf8_remaining -= 1;
            self.utf8_lower = 0x80;
            self.utf8_upper = 0xbf;
        } else {
            let (remaining, lower, upper) = match byte {
                0x00..=0x7f => (0, 0x80, 0xbf),
                0xc2..=0xdf => (1, 0x80, 0xbf),
                0xe0 => (2, 0xa0, 0xbf),
                0xe1..=0xec | 0xee..=0xef => (2, 0x80, 0xbf),
                0xed => (2, 0x80, 0x9f),
                0xf0 => (3, 0x90, 0xbf),
                0xf1..=0xf3 => (3, 0x80, 0xbf),
                0xf4 => (3, 0x80, 0x8f),
                _ => return Err(AdapterError::Parse("BibTeX source is not UTF-8".into())),
            };
            self.utf8_remaining = remaining;
            self.utf8_lower = lower;
            self.utf8_upper = upper;
        }
        self.offset += 1;
        if byte == b'\n' {
            self.line += 1;
        }
        Ok(Some(byte))
    }

    fn append(&self, bytes: &mut Vec<u8>, byte: u8) -> Result<(), AdapterError> {
        if bytes.len() == self.limits.entry_bytes {
            return Err(limit_error("raw entry", self.limits.entry_bytes));
        }
        bytes.push(byte);
        Ok(())
    }

    pub fn next_frame(&mut self) -> Result<Option<Frame>, AdapterError> {
        let mut comment = false;
        let (offset, line) = loop {
            let position = (self.offset, self.line);
            let Some(byte) = self.take()? else {
                return Ok(None);
            };
            if comment {
                comment = byte != b'\n';
            } else if byte == b'%' {
                comment = true;
            } else if byte == b'@' {
                break position;
            }
        };
        let mut bytes = Vec::new();
        self.append(&mut bytes, b'@')?;
        let mut kind = Vec::new();
        let mut kind_done = false;
        let opening = loop {
            let Some(byte) = self.take()? else {
                let regular = !kind.eq_ignore_ascii_case(b"comment")
                    && !kind.eq_ignore_ascii_case(b"string")
                    && !kind.eq_ignore_ascii_case(b"preamble");
                return Self::frame(bytes, line, offset, regular, false).map(Some);
            };
            self.append(&mut bytes, byte)?;
            if comment {
                comment = byte != b'\n';
            } else if byte == b'%' {
                comment = true;
                kind_done |= !kind.is_empty();
            } else if matches!(byte, b'{' | b'(') {
                break byte;
            } else if byte.is_ascii_whitespace() {
                kind_done |= !kind.is_empty();
            } else if !kind_done {
                kind.push(byte);
            }
        };
        let comment_entry = kind.eq_ignore_ascii_case(b"comment");
        let regular = !comment_entry
            && !kind.eq_ignore_ascii_case(b"string")
            && !kind.eq_ignore_ascii_case(b"preamble");
        let closing = if opening == b'{' { b'}' } else { b')' };
        let mut depth = 0usize;
        let mut quoted = false;
        loop {
            let Some(byte) = self.take()? else {
                return Self::frame(bytes, line, offset, regular, false).map(Some);
            };
            self.append(&mut bytes, byte)?;
            if comment {
                comment = byte != b'\n';
                continue;
            }
            if !comment_entry && !quoted && depth == 0 && byte == b'%' {
                comment = true;
                continue;
            }
            // The pinned parser treats backslashes literally for brace balancing.
            // In particular, an embedded @ can never restart an unfinished value.
            if byte == b'{' {
                depth += 1;
                if depth >= self.limits.depth {
                    return Err(limit_error("nesting depth", self.limits.depth));
                }
            } else if byte == b'}' && depth > 0 {
                depth -= 1;
            } else if !comment_entry && byte == b'"' && depth == 0 {
                quoted = !quoted;
            } else if byte == closing && depth == 0 && !quoted {
                return Self::frame(bytes, line, offset, regular, true).map(Some);
            }
        }
    }

    fn frame(
        bytes: Vec<u8>,
        line: usize,
        offset: u64,
        regular: bool,
        complete: bool,
    ) -> Result<Frame, AdapterError> {
        let source = String::from_utf8(bytes)
            .map_err(|_| AdapterError::Parse("BibTeX source is not UTF-8".into()))?;
        Ok(Frame {
            source,
            line,
            offset,
            regular,
            complete,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Read};

    struct StopAfterFrame<'a>(&'a [u8]);

    impl Read for StopAfterFrame<'_> {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            panic!("framing uses BufRead directly")
        }
    }

    impl BufRead for StopAfterFrame<'_> {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if self.0.is_empty() {
                return Err(io::Error::other("next entry has not arrived"));
            }
            Ok(self.0)
        }

        fn consume(&mut self, amount: usize) {
            self.0 = &self.0[amount..];
        }
    }

    #[test]
    fn frame_is_returned_without_reading_the_next_entry() {
        let source = b"@book{k,title={one}}";
        let mut frames = BibtexFrames::new(StopAfterFrame(source), Limits::default());
        assert_eq!(
            frames.next_frame().unwrap().unwrap().source.as_bytes(),
            source
        );
        assert!(frames.next_frame().is_err());
    }

    #[test]
    fn raw_size_and_nesting_limits_accept_the_boundary() {
        let source = b"@book{k,title={{one}}}";
        let limits = Limits {
            entry_bytes: source.len(),
            depth: 3,
            ..Limits::default()
        };
        assert!(BibtexFrames::new(source.as_slice(), limits)
            .next_frame()
            .is_ok());
        for limits in [
            Limits {
                entry_bytes: source.len() - 1,
                ..limits
            },
            Limits { depth: 2, ..limits },
        ] {
            assert!(BibtexFrames::new(source.as_slice(), limits)
                .next_frame()
                .is_err());
        }
    }
}
