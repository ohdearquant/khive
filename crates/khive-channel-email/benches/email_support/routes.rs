use std::collections::HashMap;
use std::hint::black_box;

use khive_channel_email::connector::RawEmail;
use khive_channel_email::{
    external_delivery_marking, route_inbound, ExternalDeliveryMarking, ExternalMarkingError,
    InboundRoute, InboundRoutes, ThreadOwner,
};

pub const EMAILS: usize = 1_000;
pub const ROUTES: [&str; 3] = ["thread_matched", "alias_matched", "unmatched"];
pub const MARKINGS: [&str; 3] = ["plain", "withheld_labels", "maintainer_refused"];
const EXTERNAL_ACTOR: &str = "channel:email-benchmark";

pub struct RouteFixture {
    emails: Vec<RawEmail>,
    owners: HashMap<String, ThreadOwner>,
    routes: InboundRoutes,
    expected: Vec<InboundRoute>,
}

impl RouteFixture {
    pub fn new(scene: &str) -> Self {
        assert!(ROUTES.contains(&scene));
        let mut owners = HashMap::new();
        let mut emails = Vec::with_capacity(EMAILS);
        let mut expected = Vec::with_capacity(EMAILS);
        for index in 0..EMAILS {
            let thread_id = uuid::Uuid::from_u128(index as u128 + 1).to_string();
            let actor = format!("bench:owner-{index:04}");
            let mut headers = HashMap::new();
            let thread_header = if scene == "thread_matched" {
                headers.insert("x-khive-thread-id".to_string(), thread_id.clone());
                owners.insert(
                    thread_id.clone(),
                    ThreadOwner {
                        actor: actor.clone(),
                        thread_id: thread_id.clone(),
                    },
                );
                format!("X-Khive-Thread-ID: {thread_id}\r\n")
            } else {
                String::new()
            };
            let to = if scene == "unmatched" {
                "unrouted@example.invalid"
            } else {
                "support@example.invalid"
            };
            let from = format!("sender-{index:04}@example.invalid");
            let subject = format!("Generated route fixture {index:04}");
            headers.insert("from".to_string(), from.clone());
            headers.insert("to".to_string(), to.to_string());
            headers.insert("subject".to_string(), subject.clone());
            headers.insert(
                "content-type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            );
            let raw = format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\n{thread_header}Content-Type: text/plain; charset=utf-8\r\n\r\nOrdinary generated message.\r\n");
            assert!(mail_parser::MessageParser::default()
                .parse(raw.as_bytes())
                .is_some());
            emails.push(RawEmail {
                uid: index as u32 + 1,
                raw_bytes: raw.into_bytes(),
                imap_external_id: format!("imap:fixture.invalid:1:{}", index + 1),
                from_addrs: vec![from],
                sender_addr: None,
                to: vec![to.to_string()],
                subject,
                date: None,
                body_text: Some("Ordinary generated message.".to_string()),
                body_html: None,
                headers,
                authentication_results: Vec::new(),
            });
            expected.push(match scene {
                "thread_matched" => InboundRoute::Owner { actor, thread_id },
                "alias_matched" => InboundRoute::RoutedAlias {
                    actor: "bench:support".to_string(),
                    address: to.to_string(),
                },
                "unmatched" => InboundRoute::ExternalInbound {
                    actor: EXTERNAL_ACTOR.to_string(),
                },
                _ => unreachable!(),
            });
        }
        Self {
            emails,
            owners,
            routes: InboundRoutes::parse("support@example.invalid=bench:support")
                .expect("generated route"),
            expected,
        }
    }

    pub fn run(&self, output: &mut Vec<InboundRoute>) {
        for email in &self.emails {
            output.push(route_inbound(
                black_box(email),
                &self.routes,
                EXTERNAL_ACTOR,
                |id| self.owners.get(id).cloned(),
            ));
        }
    }

    pub fn validate(&self, output: &[InboundRoute]) {
        assert_eq!(output, self.expected);
    }
}

pub type Marking = Result<ExternalDeliveryMarking, ExternalMarkingError>;

pub struct MarkingFixture {
    senders: Vec<String>,
    labels: Vec<&'static str>,
    expected: Vec<Marking>,
}

impl MarkingFixture {
    pub fn new(scene: &str) -> Self {
        assert!(MARKINGS.contains(&scene));
        let labels = if scene == "withheld_labels" {
            vec!["action_request.install_or_execute"]
        } else {
            Vec::new()
        };
        let mut senders = Vec::with_capacity(EMAILS);
        let mut expected = Vec::with_capacity(EMAILS);
        for index in 0..EMAILS {
            let from = if scene == "maintainer_refused" {
                "Maintainer@Example.Invalid".to_string()
            } else {
                format!("Sender-{index:04}@Example.Invalid")
            };
            expected.push(if scene == "maintainer_refused" {
                Err(ExternalMarkingError::MaintainerAddress {
                    address: "maintainer@example.invalid".to_string(),
                })
            } else {
                Ok(ExternalDeliveryMarking {
                    sender: format!("email:sender-{index:04}@example.invalid"),
                    origin: "external",
                    trust: "untrusted",
                    labels: labels.iter().map(|label| (*label).to_string()).collect(),
                })
            });
            senders.push(from);
        }
        Self {
            senders,
            labels,
            expected,
        }
    }

    pub fn run(&self, output: &mut Vec<Marking>) {
        for from in &self.senders {
            output.push(external_delivery_marking(
                black_box(from),
                &self.labels,
                "bench:maintainer",
                "maintainer@example.invalid",
            ));
        }
    }

    pub fn validate(&self, output: &[Marking]) {
        assert_eq!(output, self.expected);
    }
}
