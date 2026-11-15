//! Decoded views of the escaped JSON string literals inside a field.
//!
//! The detectors read raw text, so a credential written with one JSON escape
//! (`AKIA...`, `https:\/\/...`) reads as something else until it is
//! decoded. Only strings that carry an escape are returned: an unescaped
//! string already reached the detectors byte for byte.

/// Return the decoded value of every JSON string literal in `text` that
/// contains a backslash escape, whether `text` is a whole JSON document, a
/// fragment inside prose, or prose with quoted runs.
///
/// An opening quote may follow a stray prose backslash. Escaped quotes inside
/// a candidate remain content, but an invalid fragment lets the next quote
/// start a new candidate. Fragment validation and complete candidates each
/// cover disjoint runs, keeping total work linear. Invalid escapes separate
/// valid spans instead of joining their contents. Each emitted span still
/// passes strict JSON decoding; paths like `C:\Users` do not decode.
pub(super) fn escaped_strings(text: &str) -> Vec<String> {
    escaped_strings_inner(text, &mut RecoveryWork::default())
}

fn escaped_strings_inner(text: &str, work: &mut RecoveryWork) -> Vec<String> {
    if !text.contains('\\') {
        return Vec::new();
    }
    let Some(mut start) = text.find('"') else {
        return Vec::new();
    };
    let mut previous_quote = start;
    let mut decoded = Vec::new();
    let mut backslashes = 0usize;
    for (i, &b) in text.as_bytes().iter().enumerate().skip(start + 1) {
        if b == b'"' {
            if backslashes.is_multiple_of(2) {
                let literal = &text[start..=i];
                if literal.contains('\\') {
                    if let Ok(value) = serde_json::from_str::<String>(literal) {
                        decoded.push(value);
                    } else {
                        recover_valid_spans(&text[start + 1..i], &mut decoded, work);
                    }
                }
                start = i;
            } else {
                // An escaped quote completes an escape, so no JSON escape can
                // straddle this boundary. A synthetic closer validates just
                // this fragment without repeatedly parsing its earlier prefix.
                let fragment = format!("{}\"", &text[previous_quote..=i]);
                if serde_json::from_str::<String>(&fragment).is_err() {
                    // Include this escaped quote as content before reusing it
                    // as the next candidate's delimiter.
                    recover_valid_spans(&text[start + 1..=i], &mut decoded, work);
                    start = i;
                }
            }
            previous_quote = i;
        }
        backslashes = if b == b'\\' { backslashes + 1 } else { 0 };
    }
    // Complete the one remaining candidate once, preserving strict escape
    // validation. This adds one linear suffix pass, not a retry per quote.
    let tail = &text[start..];
    if tail.contains('\\') {
        let literal = format!("{tail}\"");
        if let Ok(value) = serde_json::from_str::<String>(&literal) {
            decoded.push(value);
        } else {
            recover_valid_spans(&text[start + 1..], &mut decoded, work);
        }
    }
    decoded
}

fn recover_valid_spans(interior: &str, decoded: &mut Vec<String>, work: &mut RecoveryWork) {
    work.begin_pass();
    let mut cursor = 0;
    let mut start = 0;
    let mut escaped = false;
    while cursor < interior.len() {
        let (len, valid) = recovery_token(&interior[cursor..], work);
        if valid {
            escaped |= interior.as_bytes()[cursor] == b'\\';
        } else {
            emit_span(&interior[start..cursor], escaped, decoded);
            start = cursor + len;
            escaped = false;
        }
        cursor += len;
    }
    emit_span(&interior[start..], escaped, decoded);
}

fn emit_span(span: &str, escaped: bool, decoded: &mut Vec<String>) {
    if escaped {
        let literal = format!("\"{span}\"");
        if let Ok(value) = serde_json::from_str::<String>(&literal) {
            decoded.push(value);
        }
    }
}

fn recovery_token(text: &str, work: &mut RecoveryWork) -> (usize, bool) {
    work.visit_token();
    let first = text.chars().next().expect("nonempty recovery suffix");
    if first != '\\' {
        return (first.len_utf8(), first >= ' ' && first != '"');
    }
    let Some(escaped) = text[1..].chars().next() else {
        return (1, false);
    };
    match escaped {
        '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' => (2, true),
        'u' => {
            let (len, scalar) = unicode_escape(text);
            match scalar {
                Some(0xD800..=0xDBFF) => {
                    // A nonmatching next escape belongs to the next span;
                    // consuming it here could hide an independently valid value.
                    let next = &text[len..];
                    if next.starts_with("\\u") {
                        let (next_len, low) = unicode_escape(next);
                        if matches!(low, Some(0xDC00..=0xDFFF)) {
                            return (len + next_len, true);
                        }
                    }
                    (len, false)
                }
                Some(0xDC00..=0xDFFF) | None => (len, false),
                Some(_) => (len, true),
            }
        }
        _ => (1 + escaped.len_utf8(), false),
    }
}

fn unicode_escape(text: &str) -> (usize, Option<u16>) {
    let mut len = 2;
    let mut value = 0;
    for &byte in text.as_bytes()[2..].iter().take(4) {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => break,
        };
        value = value * 16 + u16::from(digit);
        len += 1;
    }
    (len, (len == 6).then_some(value))
}

// Test accounting follows the real classifier, including repeated visits in a
// rescan. Non-test builds have neither counters nor an input-work limit.
#[derive(Default)]
pub(super) struct RecoveryWork {
    #[cfg(test)]
    pub(super) passes: usize,
    #[cfg(test)]
    pub(super) tokens: usize,
    #[cfg(test)]
    max_tokens: Option<usize>,
}

impl RecoveryWork {
    fn begin_pass(&mut self) {
        #[cfg(test)]
        {
            self.passes += 1;
        }
    }

    fn visit_token(&mut self) {
        #[cfg(test)]
        {
            self.tokens += 1;
            if let Some(limit) = self.max_tokens {
                assert!(
                    self.tokens <= limit,
                    "recovery token visits exceeded {limit}"
                );
            }
        }
    }
}

#[cfg(test)]
pub(super) fn escaped_strings_with_work(
    text: &str,
    max_tokens: usize,
) -> (Vec<String>, RecoveryWork) {
    let mut work = RecoveryWork {
        max_tokens: Some(max_tokens),
        ..RecoveryWork::default()
    };
    let decoded = escaped_strings_inner(text, &mut work);
    (decoded, work)
}
