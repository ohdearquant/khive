//! Integration tests exercising the public API as an external consumer would.

use khive_text::preset;
use khive_text::{Analyzer, BoxedAnalyzer, BoxedTokenizer, TokenFilter, Tokenizer};

// ---------------------------------------------------------------------------
// Trait-object aliases compile and are usable
// ---------------------------------------------------------------------------

#[test]
fn boxed_analyzer_is_object_safe() {
    let a: BoxedAnalyzer = std::sync::Arc::new(preset::standard());
    let tokens = a.analyze("The quick brown fox");
    assert!(tokens.contains(&"quick".to_string()));
}

#[test]
fn boxed_tokenizer_is_object_safe() {
    let t: BoxedTokenizer = std::sync::Arc::new(khive_text::tokenizer::WhitespaceTokenizer);
    let tokens = t.tokenize("hello world");
    assert_eq!(tokens, vec!["hello", "world"]);
}

// ---------------------------------------------------------------------------
// Presets produce expected output through the public surface
// ---------------------------------------------------------------------------

#[test]
fn standard_preset_filters_stops_and_short() {
    // "am" is not a BM25 default stop word, so it survives under the
    // BM25-parity contract for preset::standard().
    let a = preset::standard();
    let tokens = a.analyze("I am a test of the standard analyzer");
    assert_eq!(tokens, vec!["am", "test", "standard", "analyzer"]);
}

#[test]
fn simple_preset_keeps_stops() {
    let a = preset::simple();
    let tokens = a.analyze("I am a test");
    assert_eq!(tokens, vec!["i", "am", "a", "test"]);
}

#[test]
fn keyword_preset_single_token() {
    let a = preset::keyword();
    let tokens = a.analyze("multi word phrase");
    assert_eq!(tokens, vec!["multi word phrase"]);
}

#[test]
fn cjk_preset_unigrams() {
    let a = preset::cjk();
    let tokens = a.analyze("hello你好");
    assert!(tokens.contains(&"hello".to_string()));
    assert!(tokens.contains(&"你".to_string()));
    assert!(tokens.contains(&"好".to_string()));
}

#[test]
fn kg_name_preset_splits_identifiers() {
    let a = preset::kg_name();
    let tokens = a.analyze("camelCase");
    assert!(tokens.contains(&"camelcase".to_string()));
    assert!(tokens.contains(&"camel".to_string()));
    assert!(tokens.contains(&"case".to_string()));
}

#[test]
fn acronym_boundaries_reach_the_public_identifier_tokenizer() {
    use khive_text::identifier::is_identifier;
    use khive_text::tokenizer::IdentifierTokenizer;

    let tokenizer = IdentifierTokenizer::default();
    for (input, expected) in [
        ("XMLParser", vec!["xmlparser", "xml", "parser"]),
        ("HTTPServer", vec!["httpserver", "http", "server"]),
        ("IDValue", vec!["idvalue", "id", "value"]),
    ] {
        assert!(is_identifier(input), "{input}");
        assert_eq!(tokenizer.tokenize(input), expected, "{input}");
        assert_eq!(preset::kg_name().analyze(input), expected, "{input}");
    }
    assert_eq!(
        tokenizer.tokenize("(XMLParser), HTTPServer!"),
        vec!["xmlparser", "xml", "parser", "httpserver", "http", "server"]
    );
}

#[test]
fn acronym_parts_obey_the_configured_minimum_without_dropping_the_original() {
    use khive_text::tokenizer::IdentifierTokenizer;

    assert_eq!(
        IdentifierTokenizer { min_part_len: 4 }.tokenize("XMLParser"),
        vec!["xmlparser", "parser"]
    );
    assert_eq!(
        IdentifierTokenizer { min_part_len: 7 }.tokenize("XMLParser"),
        vec!["xmlparser"]
    );
}

#[test]
fn plural_acronyms_stay_whole_through_public_tokenizers() {
    use khive_text::identifier::{is_identifier, split_identifier};
    use khive_text::tokenizer::IdentifierTokenizer;

    let tokenizer = IdentifierTokenizer::default();
    for input in ["APIs", "IDs", "URLs"] {
        assert!(!is_identifier(input), "{input}");
        assert_eq!(split_identifier(input, 1), vec![input.to_lowercase()]);
        assert_eq!(tokenizer.tokenize(input), vec![input]);
        assert_eq!(preset::kg_name().analyze(input), vec![input.to_lowercase()]);
    }
    assert_eq!(preset::kg_name().analyze("REST APIs"), vec!["rest", "apis"]);
    assert_eq!(preset::kg_name().analyze("IDs URLs"), vec!["ids", "urls"]);
    assert_eq!(tokenizer.tokenize("(APIs), URLs!"), vec!["APIs", "URLs"]);
}

#[test]
fn plural_suffixes_do_not_hide_adjacent_identifier_boundaries() {
    use khive_text::identifier::{is_identifier, split_identifier};
    use khive_text::tokenizer::IdentifierTokenizer;

    for (input, parts) in [
        ("APIsClient", vec!["apis", "client"]),
        ("IDs_URLs", vec!["ids", "urls"]),
        ("APIs2", vec!["apis", "2"]),
        ("XMLParser", vec!["xml", "parser"]),
        ("HTTPServer", vec!["http", "server"]),
    ] {
        assert!(is_identifier(input), "{input}");
        assert_eq!(split_identifier(input, 1), parts, "{input}");
    }
    assert_eq!(
        IdentifierTokenizer { min_part_len: 4 }.tokenize("IDs_URLs"),
        vec!["ids_urls", "urls"]
    );
}

#[test]
fn acronym_detection_preserves_plain_word_and_whitespace_rules() {
    use khive_text::identifier::is_identifier;
    use khive_text::tokenizer::IdentifierTokenizer;

    let tokenizer = IdentifierTokenizer::default();
    for word in ["", "X", "XML", "Parser", "plain", "\u{C9}Parser", "123"] {
        assert!(!is_identifier(word), "{word}");
        if !word.is_empty() {
            assert_eq!(tokenizer.tokenize(word), vec![word], "{word}");
        }
    }
    assert!(!is_identifier("XMLParser HTTPServer"));
    assert!(!is_identifier("XMLParser\u{2003}HTTPServer"));
    assert_eq!(tokenizer.tokenize("XML Parser"), vec!["XML", "Parser"]);
}

// ---------------------------------------------------------------------------
// Re-exported items are accessible
// ---------------------------------------------------------------------------

#[test]
fn standard_analyzer_reexport() {
    let _a =
        khive_text::StandardAnalyzer::with_tokenizer(khive_text::tokenizer::WhitespaceTokenizer);
}

#[test]
fn script_profile_reexport() {
    let p = khive_text::ScriptProfile::analyze("hello");
    assert_eq!(p.char_count, 5);
}

#[test]
fn is_meaningful_query_reexport() {
    assert!(khive_text::is_meaningful_query("search term"));
    assert!(!khive_text::is_meaningful_query(""));
}

#[test]
fn contains_cjk_reexport() {
    assert!(khive_text::contains_cjk("你好世界"));
    assert!(!khive_text::contains_cjk("hello"));
}

#[test]
fn is_cjk_char_reexport() {
    assert!(khive_text::is_cjk_char('你'));
    assert!(!khive_text::is_cjk_char('a'));
}

// ---------------------------------------------------------------------------
// TokenFilter trait is accessible on public filter types
// ---------------------------------------------------------------------------

#[test]
fn lowercase_filter_public() {
    let f = khive_text::filter::LowercaseFilter;
    assert_eq!(f.apply("HELLO".to_string()), Some("hello".to_string()));
}

#[test]
fn min_length_filter_public() {
    let f = khive_text::filter::MinLengthFilter(3);
    assert_eq!(f.apply("ab".to_string()), None);
    assert_eq!(f.apply("abc".to_string()), Some("abc".to_string()));
}

#[test]
fn max_length_filter_public() {
    let f = khive_text::filter::MaxLengthFilter(5);
    assert_eq!(f.apply("hello".to_string()), Some("hello".to_string()));
    assert_eq!(f.apply("toolong".to_string()), None);
}

#[test]
fn stop_word_filter_public() {
    let f = khive_text::filter::StopWordFilter;
    assert_eq!(f.apply("the".to_string()), None);
    assert_eq!(
        f.apply("transformer".to_string()),
        Some("transformer".to_string())
    );
}

// ---------------------------------------------------------------------------
// WhitespaceTokenizer splits on Unicode whitespace (not just ASCII)
// ---------------------------------------------------------------------------

#[test]
fn whitespace_tokenizer_splits_unicode_whitespace() {
    let t = khive_text::tokenizer::WhitespaceTokenizer;
    // U+2003 EM SPACE is Unicode whitespace, not ASCII
    let tokens = t.tokenize("hello\u{2003}world");
    assert_eq!(tokens, vec!["hello", "world"]);
}
