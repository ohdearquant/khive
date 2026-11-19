# Shared session helpers

Session mirror ingestion and the versioned identity migration use
`khive_types::hash::framed_text_sha256`. The optional `sha2` feature supplies a
`no_std`-compatible implementation without adding a new resolved package.

The persisted lowercase hexadecimal digest is unchanged: a presence byte and,
when present, a big-endian `u64` UTF-8 byte length frame the parsed text. A second
big-endian `u64` byte length frames the exact raw record. Neither value is
normalized. Absent text, empty text, embedded delimiters and Unicode values keep
their existing identity. Existing rows do not need a backfill.

Session list and search share inclusive limit validation while retaining their
own defaults, maxima, error messages and validation order. Their existing handler
tests remain in place.

The other helper groups already have shared implementations: the provider export
wrappers delegate to `mirror_whole_file_export` with `WholeFileExportSpec`, and
session responses use the runtime's `micros_to_iso`. The provider-specific parser,
source label, byte ceiling and checked file identity remain supplied by each
caller.
