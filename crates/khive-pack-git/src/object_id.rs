/// A 40-byte ASCII-hex string, accepting either case for a full git commit SHA-1.
pub(crate) fn is_40_hex(value: &[u8]) -> bool {
    value.len() == 40 && value.iter().all(u8::is_ascii_hexdigit)
}

#[cfg(test)]
#[path = "object_id_tests.rs"]
mod tests;
