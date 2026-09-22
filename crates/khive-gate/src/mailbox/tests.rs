use std::sync::Arc;

use khive_types::Namespace;
use serde_json::json;

use super::*;
use crate::{AllowAllGate, CallerEnrollmentGate, Obligation};

fn request(actor: ActorRef, verb: &str, args: serde_json::Value) -> GateRequest {
    GateRequest::new(actor, Namespace::local(), verb, args)
}

fn policy(inner: GateRef) -> MailboxReadGate {
    MailboxReadGate::new(
        inner,
        ActorRef::new("actor", "lambda:owner"),
        vec![ActorRef::new("actor", "lambda:owner:reader")],
    )
    .unwrap()
}

fn assert_denied(result: Result<GateDecision, GateError>) {
    assert!(
        matches!(result.unwrap(), GateDecision::Deny { reason } if reason == "mailbox_read_not_granted")
    );
}

#[test]
fn cross_mailbox_requires_exact_structural_pair_even_with_allow_all() {
    let gate = policy(Arc::new(AllowAllGate));
    for verb in ["comm.inbox", "comm.thread"] {
        let req = request(
            ActorRef::new("actor", "lambda:owner:reader"),
            verb,
            json!({"mailbox_actor":"lambda:owner"}),
        );
        assert_denied(check_with_mailbox_policy(&AllowAllGate, &req));
        assert!(check_with_mailbox_policy(&gate, &req).unwrap().is_allow());
        for actor in [
            ActorRef::new("actor", "lambda:owner:reader:child"),
            ActorRef::new("actor", "lambda:owner:Reader"),
            ActorRef::new("agent", "lambda:owner:reader"),
            ActorRef::new("lambda", "owner:reader"),
            ActorRef::anonymous(),
            ActorRef::new("actor", "local"),
        ] {
            let mut changed = req.clone();
            changed.actor = actor;
            // A namespace or caller-provided identity never supplies a grant.
            changed.namespace = Namespace::parse("lambda:owner").unwrap();
            changed.args["actor"] = json!("lambda:owner:reader");
            assert_denied(check_with_mailbox_policy(&gate, &changed));
        }
        let mut other_owner = req.clone();
        other_owner.args["mailbox_actor"] = json!("lambda:owner:child");
        assert_denied(check_with_mailbox_policy(&gate, &other_owner));
    }
}

#[test]
fn own_and_omitted_selectors_preserve_ordinary_policy() {
    for actor in [
        ActorRef::anonymous(),
        ActorRef::new("actor", "lambda:owner"),
    ] {
        let req = request(actor, "comm.inbox", json!({}));
        assert!(check_with_mailbox_policy(&AllowAllGate, &req)
            .unwrap()
            .is_allow());
    }
    let own = request(
        ActorRef::new("actor", "lambda:owner"),
        "comm.thread",
        json!({"mailbox_actor":"lambda:owner"}),
    );
    assert!(check_with_mailbox_policy(&AllowAllGate, &own)
        .unwrap()
        .is_allow());
    for value in [
        json!(null),
        json!(7),
        json!("local"),
        json!(" "),
        json!("a\nb"),
        json!("x".repeat(256)),
    ] {
        let req = request(
            own.actor.clone(),
            "comm.inbox",
            json!({"mailbox_actor":value}),
        );
        assert_eq!(
            mailbox_read_owner(&req).unwrap_err(),
            MailboxPolicyError::InvalidSelector
        );
    }
}

#[derive(Debug)]
struct ObligatedGate;

impl Gate for ObligatedGate {
    fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(GateDecision::allow_with(vec![Obligation::Audit {
            tag: "base-policy".into(),
        }]))
    }
}

#[derive(Debug)]
struct BrokenGate;

impl Gate for BrokenGate {
    fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
        Err(GateError::Internal("backend failure".into()))
    }
}

#[derive(Debug)]
struct BrokenMailboxGate;

impl Gate for BrokenMailboxGate {
    fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(GateDecision::allow())
    }
    fn check_mailbox_read(
        &self,
        _req: &GateRequest,
        _owner: &ActorRef,
    ) -> Result<GateDecision, GateError> {
        Err(GateError::Internal("mailbox backend failure".into()))
    }
}

#[test]
fn mailbox_grant_cannot_override_base_denial_failure_or_obligations() {
    let req = request(
        ActorRef::new("actor", "lambda:owner:reader"),
        "comm.inbox",
        json!({"mailbox_actor":"lambda:owner"}),
    );
    let denied = policy(Arc::new(CallerEnrollmentGate::new(vec![], false)));
    assert!(
        matches!(check_with_mailbox_policy(&denied, &req).unwrap(), GateDecision::Deny { reason } if reason == "actor is not enrolled")
    );
    assert!(matches!(
        check_with_mailbox_policy(&policy(Arc::new(BrokenGate)), &req),
        Err(GateError::Internal(_))
    ));
    assert!(matches!(
        check_with_mailbox_policy(&BrokenMailboxGate, &req),
        Err(GateError::Internal(_))
    ));
    let decision = check_with_mailbox_policy(&policy(Arc::new(ObligatedGate)), &req).unwrap();
    let GateDecision::Allow { obligations } = decision else {
        panic!("expected allow")
    };
    assert_eq!(obligations.len(), 1);
    assert!(matches!(&obligations[0], Obligation::Audit { tag } if tag == "base-policy"));
    // Dedicated permission is read-only and cannot be used for marking/replying.
    for verb in ["comm.read", "comm.mark_read", "comm.reply", "authorize"] {
        let mut req = req.clone();
        req.verb = verb.into();
        assert_denied(
            policy(Arc::new(AllowAllGate))
                .check_mailbox_read(&req, &ActorRef::new("actor", "lambda:owner")),
        );
    }
}

#[test]
fn policy_validation_and_fingerprint_are_bounded_and_structural() {
    let make = |owner: ActorRef, readers: Vec<ActorRef>| {
        MailboxReadGate::new(Arc::new(AllowAllGate), owner, readers)
    };
    let owner = ActorRef::new("actor", "lambda:owner");
    let one = ActorRef::new("actor", "lambda:owner:reader");
    let two = ActorRef::new("actor", "助手/检查者");
    let a = make(owner.clone(), vec![one.clone(), two.clone()]).unwrap();
    let b = make(owner.clone(), vec![two.clone(), one.clone(), one.clone()]).unwrap();
    assert_eq!(a.configuration_fingerprint(), b.configuration_fingerprint());
    let req = request(two, "comm.inbox", json!({"mailbox_actor":"lambda:owner"}));
    assert!(a.check(&req).unwrap().is_allow());
    for different in [
        make(owner.clone(), vec![one.clone()]).unwrap(),
        make(ActorRef::new("actor", "other"), vec![one.clone()]).unwrap(),
        make(owner.clone(), vec![ActorRef::new("agent", one.id.clone())]).unwrap(),
        MailboxReadGate::new(
            Arc::new(CallerEnrollmentGate::new(vec![one.id.clone()], false)),
            owner.clone(),
            vec![one.clone()],
        )
        .unwrap(),
    ] {
        assert_ne!(
            a.configuration_fingerprint(),
            different.configuration_fingerprint()
        );
    }
    assert!(matches!(
        make(ActorRef::anonymous(), vec![]),
        Err(MailboxPolicyError::InvalidOwner)
    ));
    for invalid in [
        ActorRef::anonymous(),
        ActorRef::new("actor", "local"),
        ActorRef::new("actor", " "),
        ActorRef::new("actor", "bad\nlabel"),
        ActorRef::new("actor", "x".repeat(256)),
    ] {
        assert!(matches!(
            make(owner.clone(), vec![invalid]),
            Err(MailboxPolicyError::InvalidReader)
        ));
    }
    assert!(make(owner.clone(), vec![one.clone(); 256]).is_ok());
    assert!(matches!(
        make(owner, vec![one; 257]),
        Err(MailboxPolicyError::TooManyReaders)
    ));
}
