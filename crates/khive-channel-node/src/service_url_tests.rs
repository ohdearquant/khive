//! Tests for the shared service URL check used by configuration activation and the client.

use crate::client::validate_service_url;

#[test]
fn a_plain_https_origin_is_accepted() {
    validate_service_url("https://channel.example.com").expect("https origin");
}

#[test]
fn plain_http_is_refused() {
    assert!(validate_service_url("http://channel.example.com").is_err());
}

#[test]
fn credentials_are_refused() {
    assert!(validate_service_url("https://user:secret@channel.example.com").is_err());
}

#[test]
fn service_url_with_a_query_is_refused() {
    assert!(validate_service_url("https://channel.example.com/?a=1").is_err());
}

#[test]
fn service_url_with_a_fragment_is_refused() {
    assert!(validate_service_url("https://channel.example.com/#frag").is_err());
}
