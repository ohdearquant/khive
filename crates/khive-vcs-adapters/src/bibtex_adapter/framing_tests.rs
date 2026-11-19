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
