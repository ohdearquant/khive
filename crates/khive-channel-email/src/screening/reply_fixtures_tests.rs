//! Screening outcomes for recorded replies to an outreach email (ADR-200 section 8.3, acceptance A15).
//!
//! Five fixtures are ordinary developer replies that the rules must deliver, including one that
//! suggests installing a package, one that asks for a trial key, and one that describes a server
//! endpoint. One fixture is a hostile control addressed to an automated reader that two families
//! must quarantine. A decline whose wording is not in the opt-out phrase list is delivered without
//! the opt-out label, so nothing suppresses the sender automatically.

use mail_parser::MessageParser;

use super::{screen_message, Disposition, Family, ScreeningOutcome};

fn reply_fixture(name: &str) -> &'static [u8] {
    match name {
        "reply-engaged-install" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/reply-engaged-install.eml.rs");
            }
            recorded::MIME
        }
        "reply-engaged-trial-key" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/reply-engaged-trial-key.eml.rs");
            }
            recorded::MIME
        }
        "reply-engaged-endpoint" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/reply-engaged-endpoint.eml.rs");
            }
            recorded::MIME
        }
        "reply-decline-unlisted-opt-out" => {
            mod recorded {
                include!(
                    "../../tests/fixtures/external-inbound/reply-decline-unlisted-opt-out.eml.rs"
                );
            }
            recorded::MIME
        }
        "reply-pricing-question" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/reply-pricing-question.eml.rs");
            }
            recorded::MIME
        }
        "reply-hostile-two-families" => {
            mod recorded {
                include!("../../tests/fixtures/external-inbound/reply-hostile-two-families.eml.rs");
            }
            recorded::MIME
        }
        other => panic!("unknown reply fixture {other}"),
    }
}

fn screen_fixture(name: &str) -> ScreeningOutcome {
    let raw = reply_fixture(name);
    let message = MessageParser::default()
        .parse(raw)
        .unwrap_or_else(|| panic!("fixture {name} did not parse as MIME"));
    screen_message(&message).expect("compiled-in screening rules are valid")
}

fn assert_delivered_without_opt_out(name: &str) {
    let outcome = screen_fixture(name);
    assert_eq!(
        outcome.disposition,
        Disposition::DeliverMarked,
        "{name}: families {:?}, rules {:?}",
        outcome.families,
        outcome.rule_ids
    );
    assert!(
        !outcome.opt_out,
        "{name}: opt-out matched through rules {:?}",
        outcome.rule_ids
    );
    assert!(
        outcome.families.is_empty(),
        "{name}: families {:?}",
        outcome.families
    );
}

#[test]
fn a15_reply_suggesting_a_package_install_is_delivered() {
    assert_delivered_without_opt_out("reply-engaged-install");
}

#[test]
fn a15_reply_asking_for_a_trial_key_is_delivered() {
    assert_delivered_without_opt_out("reply-engaged-trial-key");
}

#[test]
fn a15_reply_describing_a_server_endpoint_is_delivered() {
    assert_delivered_without_opt_out("reply-engaged-endpoint");
}

#[test]
fn a15_decline_outside_the_opt_out_phrase_list_is_delivered_unlabelled() {
    assert_delivered_without_opt_out("reply-decline-unlisted-opt-out");
}

#[test]
fn a15_pricing_question_is_delivered() {
    assert_delivered_without_opt_out("reply-pricing-question");
}

#[test]
fn a15_hostile_reply_quarantines_on_two_families() {
    let outcome = screen_fixture("reply-hostile-two-families");
    assert_eq!(
        outcome.disposition,
        Disposition::QuarantineScreened,
        "families {:?}, rules {:?}",
        outcome.families,
        outcome.rule_ids
    );
    assert!(
        outcome.families.contains(&Family::AgentAddressed),
        "families {:?}",
        outcome.families
    );
    assert!(
        outcome.families.contains(&Family::ActionRequest),
        "families {:?}",
        outcome.families
    );
    assert!(!outcome.opt_out, "rules {:?}", outcome.rule_ids);
}
