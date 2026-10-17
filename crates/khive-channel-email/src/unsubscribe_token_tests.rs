    #[test]
    fn extracts_the_token_after_the_marker_and_whitespace() {
        assert_eq!(
            extract_unsubscribe_token("unsubscribe AQAB_-cD12"),
            Some("AQAB_-cD12")
        );
    }

    #[test]
    fn marker_match_is_case_insensitive_and_may_be_mid_subject() {
        assert_eq!(
            extract_unsubscribe_token("Re: UNSUBSCRIBE AQABtoken"),
            Some("AQABtoken")
        );
    }

    #[test]
    fn stops_the_token_at_the_first_non_base64url_character() {
        assert_eq!(
            extract_unsubscribe_token("unsubscribe abc.def"),
            Some("abc")
        );
    }

    #[test]
    fn requires_whitespace_between_marker_and_token() {
        assert_eq!(extract_unsubscribe_token("unsubscribeabc"), None);
        assert_eq!(extract_unsubscribe_token("unsubscribed AQABtoken"), None);
    }

    #[test]
    fn only_ascii_whitespace_separates_marker_and_token() {
        assert_eq!(
            extract_unsubscribe_token("unsubscribe\u{a0}AQABtoken"),
            None
        );
        assert_eq!(
            extract_unsubscribe_token("unsubscribe\u{3000}AQABtoken"),
            None
        );
        assert_eq!(
            extract_unsubscribe_token("unsubscribe\tAQABtoken"),
            Some("AQABtoken")
        );
    }

    #[test]
    fn a_marker_with_no_token_yields_nothing() {
        assert_eq!(extract_unsubscribe_token("unsubscribe"), None);
        assert_eq!(extract_unsubscribe_token("unsubscribe   "), None);
        // A word after the marker is a candidate token, not a match: it simply will not verify
        // and the message continues as ordinary mail.
        assert_eq!(
            extract_unsubscribe_token("please unsubscribe me"),
            Some("me")
        );
    }

    #[test]
    fn an_ordinary_subject_has_no_token() {
        assert_eq!(extract_unsubscribe_token("Hello there"), None);
        assert_eq!(extract_unsubscribe_token(""), None);
    }
