//! Exact label semantics, using the real MailboxReadGate composition.
use std::sync::Arc;

use khive_gate::{
    check_with_mailbox_policy, mailbox_read_owner, ActorRef, AllowAllGate, GateDecision, GateError,
    GateRequest, MailboxPolicyError, MailboxReadGate,
};
use khive_types::Namespace;
use serde_json::json;

pub struct MailboxCase {
    pub name: &'static str,
    pub request: GateRequest,
    pub expected_owner: ActorRef,
    pub allowed: bool,
}

impl MailboxCase {
    pub fn assert_owner(&self, result: Result<Option<ActorRef>, MailboxPolicyError>) {
        assert_eq!(
            result.expect("valid selector"),
            Some(self.expected_owner.clone()),
            "owner extraction does not grant access"
        );
    }

    pub fn assert_decision(&self, result: Result<GateDecision, GateError>) {
        match result.expect("policy result") {
            GateDecision::Allow { obligations } => {
                assert!(self.allowed, "ungranted request admitted");
                assert!(obligations.is_empty());
            }
            GateDecision::Deny { reason } => {
                assert!(!self.allowed, "granted reader denied");
                assert_eq!(reason, "mailbox_read_not_granted");
            }
        }
    }
}

fn request(caller: &str, owner: &str) -> GateRequest {
    GateRequest::new(
        ActorRef::new("actor", caller),
        Namespace::local(),
        "comm.inbox",
        json!({"mailbox_actor":owner}),
    )
}

pub fn fixture() -> (MailboxReadGate, Vec<MailboxCase>) {
    let gate = MailboxReadGate::new(
        Arc::new(AllowAllGate),
        ActorRef::new("actor", "lambda:owner"),
        vec![ActorRef::new("actor", "lambda:owner:reader")],
    )
    .expect("valid immutable policy");
    let cases = vec![
        MailboxCase {
            name: "matched_owner",
            request: request("lambda:owner:reader", "lambda:owner"),
            expected_owner: ActorRef::new("actor", "lambda:owner"),
            allowed: true,
        },
        MailboxCase {
            name: "unmatched_owner",
            request: request("lambda:owner:reader", "lambda:other"),
            expected_owner: ActorRef::new("actor", "lambda:other"),
            allowed: false,
        },
        MailboxCase {
            name: "denied_reader",
            request: request("lambda:owner:reader:child", "lambda:owner"),
            expected_owner: ActorRef::new("actor", "lambda:owner"),
            allowed: false,
        },
    ];
    for case in &cases {
        case.assert_owner(mailbox_read_owner(&case.request));
        case.assert_decision(check_with_mailbox_policy(&gate, &case.request));
    }
    // These are distinct parser/identity boundaries, not denial benchmark aliases.
    let own = request("lambda:owner:reader", "lambda:owner:reader");
    let mut omitted = own.clone();
    omitted.args = json!({});
    let mut unrelated = own.clone();
    unrelated.verb = "get".into();
    for no_delegate in [own, omitted, unrelated] {
        assert_eq!(mailbox_read_owner(&no_delegate).expect("no delegate"), None);
        assert!(check_with_mailbox_policy(&gate, &no_delegate)
            .expect("ordinary allow")
            .is_allow());
    }
    for invalid in [json!("local"), json!(null)] {
        let mut req = request("lambda:owner:reader", "lambda:owner");
        req.args["mailbox_actor"] = invalid;
        assert_eq!(
            mailbox_read_owner(&req),
            Err(MailboxPolicyError::InvalidSelector)
        );
        assert!(matches!(
            check_with_mailbox_policy(&gate, &req),
            Err(GateError::Policy(_))
        ));
    }
    (gate, cases)
}
