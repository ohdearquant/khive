//! Placement, cutting, request text and reassembly for ADR-220 routing.

use std::sync::Mutex;

use async_trait::async_trait;
use khive_request::{
    parse_request, ArgValue, ExecutionMode, ParsedOp, ParsedRequest, NESTING_DEPTH_LIMIT,
};
use khive_runtime::{ComputerSectionConfig, DomainDisposition};
use serde_json::{json, Map, Value};

use super::{
    execute_split, render_unit, ComputerRouting, Home, HomeOutcome, HomeSender, Placement,
    RenderError, RoutePlan, Segment, UnitPlan,
};

fn routing(packs: &[&str]) -> ComputerRouting {
    ComputerRouting::new(&ComputerSectionConfig {
        name: "build-1".to_string(),
        url: "http://127.0.0.1:8700/mcp".to_string(),
        key_file: "/nonexistent/computer-build-1.key".into(),
        packs: packs.iter().map(|pack| (*pack).to_string()).collect(),
        call_timeout_s: 3700,
    })
    .expect("the fixture table is valid")
}

fn parsed(ops: &str) -> ParsedRequest {
    parse_request(ops).unwrap_or_else(|error| panic!("fixture {ops:?} does not parse: {error:?}"))
}

fn segment(placement: Placement, ops: std::ops::Range<usize>) -> Segment {
    Segment { placement, ops }
}

#[test]
fn each_operation_is_placed_by_its_pack() {
    let r = routing(&["computer", "exec", "git"]);
    for routed in ["exec.run", "git.commit", "computer.fs_read"] {
        assert_eq!(r.place(routed), Placement::Home(Home::Computer), "{routed}");
    }
    for refused in ["code.search", "workspace.open", "tool.request"] {
        assert_eq!(r.place(refused), Placement::RunsOnComputer, "{refused}");
    }
    for store in [
        "memory.recall",
        "comm.inbox",
        "blob.get",
        "create",
        "verbs",
        "whoami",
    ] {
        assert_eq!(r.place(store), Placement::Home(Home::Store), "{store}");
    }
    let with_blob = routing(&["exec", "blob"]);
    assert_eq!(with_blob.place("blob.get"), Placement::Home(Home::Computer));
}

#[test]
fn the_label_names_the_computer_and_its_host_and_port() {
    assert_eq!(routing(&["exec"]).label(), "build-1 (127.0.0.1:8700)");
}

#[test]
fn a_request_with_one_home_is_sent_whole() {
    let r = routing(&["exec", "git"]);
    let home = |ops: &str| RoutePlan::new(&r, &parsed(ops)).whole_home();
    assert_eq!(home(r#"memory.recall(query="a")"#), Some(Home::Store));
    assert_eq!(
        home(r#"[memory.recall(query="a"), comm.inbox()]"#),
        Some(Home::Store)
    );
    assert_eq!(
        home(r#"exec.run(tool="sh") | git.log(limit=1)"#),
        Some(Home::Computer)
    );
    assert_eq!(
        home(r#"[exec.run(tool="sh"), git.log(limit=1)]"#),
        Some(Home::Computer)
    );
    assert_eq!(home(r#"[exec.run(tool="sh"), comm.inbox()]"#), None);
    assert_eq!(
        home(r#"memory.recall(query="a") | exec.run(tool="sh")"#),
        None
    );
    assert_eq!(home(r#"code.search(query="x")"#), None);
}

#[test]
fn a_chain_is_cut_into_segments_in_input_order() {
    let r = routing(&["exec", "git"]);
    let plan = RoutePlan::new(
        &r,
        &parsed(
            r#"memory.recall(query="a") | exec.run(tool="sh") | git.log(limit=1) | memory.remember(content="b") | code.search(query="c")"#,
        ),
    );
    assert_eq!(plan.mode, ExecutionMode::Chain);
    assert_eq!(
        plan.units,
        vec![UnitPlan {
            segments: vec![
                segment(Placement::Home(Home::Store), 0..1),
                segment(Placement::Home(Home::Computer), 1..3),
                segment(Placement::Home(Home::Store), 3..4),
                segment(Placement::RunsOnComputer, 4..5),
            ]
        }]
    );
}

#[test]
fn a_bracketed_batch_places_each_unit_on_its_own() {
    let r = routing(&["exec", "git"]);
    let plan = RoutePlan::new(
        &r,
        &parsed(
            r#"[comm.inbox(limit=10), exec.run(tool="sh") | git.log(limit=1), memory.recall(query="q") | exec.run(tool="zsh")]"#,
        ),
    );
    assert_eq!(plan.mode, ExecutionMode::Parallel);
    assert_eq!(
        plan.units,
        vec![
            UnitPlan {
                segments: vec![segment(Placement::Home(Home::Store), 0..1)]
            },
            UnitPlan {
                segments: vec![segment(Placement::Home(Home::Computer), 1..3)]
            },
            UnitPlan {
                segments: vec![
                    segment(Placement::Home(Home::Store), 3..4),
                    segment(Placement::Home(Home::Computer), 4..5),
                ]
            },
        ]
    );
}

fn rendered(request: &ParsedRequest) -> String {
    let units: Vec<String> = request
        .ranges
        .iter()
        .map(|range| render_unit(&request.ops[range.clone()]).expect("the fixture renders"))
        .collect();
    if request.mode == ExecutionMode::Parallel {
        format!("[{}]", units.join(", "))
    } else {
        units.join(" | ")
    }
}

#[test]
fn a_rendered_request_parses_back_to_the_same_operations() {
    let fixtures = [
        r#"exec.run(tool="sh", args=["-c", "printf \"%s\\n\" done\n\tnext"], timeout_s=600)"#,
        r#"memory.remember(content="na\u00efve \u96ea \u0007 end", salience=0.35, big=18446744073709551615, small=-1.5e-7, none=null, yes=true)"#,
        r#"memory.remember(content="\\$prev.literal", note="$prev.a]", other="\\\\$prev")"#,
        r#"get(id="n1") | exec.run(tree=$prev.tree, args=[$prev.items[0].id, "lit"], env={"A": $prev.a, "B": 1})"#,
        r#"get(id="n1") | exec.run(tree="$prev.items[0].id", x="$prev[2]", y=$prev[3].name, z=$prev)"#,
        r#"[comm.inbox(limit=10), exec.run(tool="sh") | git.log(limit=1), memory.recall(query="q")]"#,
        r#"workspace.put(meta={"nested": [[{"k": "v"}], {}], "e": []})"#,
    ];
    for ops in fixtures {
        let original = parsed(ops);
        let text = rendered(&original);
        let again = parse_request(&text).unwrap_or_else(|error| {
            panic!("{ops} rendered as {text}, which does not parse: {error:?}")
        });
        assert_eq!(again, original, "{ops} rendered as {text}");
    }
}

#[test]
fn what_the_request_language_cannot_write_is_refused() {
    let op = |name: &str, value: Value| ParsedOp {
        tool: "memory.remember".to_string(),
        args: [(name.to_string(), ArgValue::Value(value))]
            .into_iter()
            .collect(),
    };
    assert_eq!(
        render_unit(&[op("content", json!("\\$prev.x"))]),
        Err(RenderError::BackslashPrevString)
    );
    assert_eq!(
        render_unit(&[op("not-a-name", json!(1))]),
        Err(RenderError::ArgName("not-a-name".to_string()))
    );
    let nested_tool = ParsedOp {
        tool: "a.b.c".to_string(),
        args: Default::default(),
    };
    assert_eq!(
        render_unit(&[nested_tool]),
        Err(RenderError::Tool("a.b.c".to_string()))
    );
    let mut deep = json!(0);
    for _ in 0..=NESTING_DEPTH_LIMIT {
        deep = json!([deep]);
    }
    assert_eq!(
        render_unit(&[op("content", deep)]),
        Err(RenderError::TooDeep)
    );
    // A backslash that does not stand before a `$prev` form is an ordinary character.
    assert!(render_unit(&[op("content", json!("\\\\$prev"))]).is_ok());
}

type Answer = dyn Fn(Home, &ParsedRequest) -> HomeOutcome + Send + Sync;

/// Two homes that record every request and answer through `answer`.
struct FakeHomes {
    sent: Mutex<Vec<(Home, String, Vec<usize>)>>,
    answer: Box<Answer>,
}

impl FakeHomes {
    fn new(answer: impl Fn(Home, &ParsedRequest) -> HomeOutcome + Send + Sync + 'static) -> Self {
        Self {
            sent: Mutex::new(Vec::new()),
            answer: Box::new(answer),
        }
    }

    /// Every request, in the order it was sent.
    fn requests(&self) -> Vec<(Home, String, Vec<usize>)> {
        self.sent.lock().expect("the fake's lock").clone()
    }

    /// Every request, store requests first: the two homes of one batch are
    /// sent concurrently, so their order is not part of the contract.
    fn requests_by_home(&self) -> Vec<(Home, String, Vec<usize>)> {
        let mut sent = self.requests();
        sent.sort_by_key(|(home, _, _)| *home == Home::Computer);
        sent
    }
}

#[async_trait]
impl HomeSender for FakeHomes {
    async fn send(&self, home: Home, ops: String, indices: Vec<usize>) -> HomeOutcome {
        let request = parse_request(&ops)
            .unwrap_or_else(|error| panic!("routed request {ops:?} does not parse: {error:?}"));
        self.sent
            .lock()
            .expect("the fake's lock")
            .push((home, ops, indices));
        (self.answer)(home, &request)
    }
}

/// Every operation succeeds with a result that names its home and echoes its
/// literal arguments.
fn served(home: Home, request: &ParsedRequest) -> HomeOutcome {
    let results: Vec<Value> = request
        .ops
        .iter()
        .map(|op| {
            let args: Map<String, Value> = op
                .args
                .iter()
                .filter_map(|(name, arg)| arg.as_value().map(|value| (name.clone(), value.clone())))
                .collect();
            json!({
                "ok": true,
                "tool": op.tool,
                "result": {"home": format!("{home:?}"), "tree": "T-1", "receipt": "R-1", "args": args},
            })
        })
        .collect();
    HomeOutcome::Envelope(json!({"results": results, "summary": {}, "status": "success"}))
}

async fn run(r: &ComputerRouting, ops: &str, homes: &FakeHomes) -> Value {
    let request = parsed(ops);
    let plan = RoutePlan::new(r, &request);
    assert_eq!(
        plan.whole_home(),
        None,
        "{ops} has one home and is sent whole"
    );
    execute_split(r, &plan, &request, homes).await
}

fn entries(envelope: &Value) -> &Vec<Value> {
    envelope["results"].as_array().expect("a results array")
}

#[tokio::test]
async fn a_mixed_batch_is_one_request_per_home_restored_to_input_order() {
    let r = routing(&["exec", "git"]);
    let homes = FakeHomes::new(served);
    let envelope = run(
        &r,
        r#"[memory.recall(query="a"), exec.run(tool="sh"), comm.inbox(limit=5), git.log(limit=1)]"#,
        &homes,
    )
    .await;
    assert_eq!(
        homes.requests_by_home(),
        vec![
            (
                Home::Store,
                r#"[memory.recall(query="a"), comm.inbox(limit=5)]"#.to_string(),
                vec![0, 2]
            ),
            (
                Home::Computer,
                r#"[exec.run(tool="sh"), git.log(limit=1)]"#.to_string(),
                vec![1, 3]
            ),
        ]
    );
    let homes_in_order: Vec<&str> = entries(&envelope)
        .iter()
        .map(|entry| entry["result"]["home"].as_str().expect("a home"))
        .collect();
    assert_eq!(homes_in_order, ["Store", "Computer", "Store", "Computer"]);
    let tools: Vec<&str> = entries(&envelope)
        .iter()
        .map(|entry| entry["tool"].as_str().expect("a tool"))
        .collect();
    assert_eq!(
        tools,
        ["memory.recall", "exec.run", "comm.inbox", "git.log"]
    );
    assert_eq!(
        envelope["summary"],
        json!({"total": 4, "succeeded": 4, "failed": 0, "aborted": 0})
    );
    assert_eq!(envelope["status"], "success");
}

#[tokio::test]
async fn a_chain_across_homes_carries_prev_as_a_literal() {
    let r = routing(&["exec"]);
    let homes = FakeHomes::new(served);
    let envelope = run(
        &r,
        r#"get(id="n1") | exec.run(tree=$prev.tree, args=["-c", $prev.receipt]) | exec.runs(session_id=$prev.receipt)"#,
        &homes,
    )
    .await;
    assert_eq!(
        homes.requests(),
        vec![
            (Home::Store, r#"get(id="n1")"#.to_string(), vec![0]),
            (
                Home::Computer,
                r#"exec.run(args=["-c","R-1"], tree="T-1") | exec.runs(session_id="$prev.receipt")"#
                    .to_string(),
                vec![1, 2]
            ),
        ]
    );
    assert_eq!(
        envelope["summary"],
        json!({"total": 3, "succeeded": 3, "failed": 0, "aborted": 0})
    );
}

#[tokio::test]
async fn a_computer_refusal_reaches_the_caller_unchanged_and_aborts_the_rest() {
    let r = routing(&["exec"]);
    let refusal = json!({
        "ok": false,
        "tool": "exec.run",
        "error": {"kind": "grant_required", "message": "exec.run is not granted to this actor"},
        "domain_disposition": "not_committed",
        "receipt": null,
    });
    let answer = refusal.clone();
    let homes = FakeHomes::new(move |home, request| match home {
        Home::Computer => HomeOutcome::Envelope(
            json!({"results": [answer.clone()], "summary": {"total": 1}, "status": "partial"}),
        ),
        Home::Store => served(home, request),
    });
    let envelope = run(
        &r,
        r#"exec.run(tool="sh") | memory.remember(content="after")"#,
        &homes,
    )
    .await;
    assert_eq!(
        homes.requests(),
        vec![(
            Home::Computer,
            r#"exec.run(tool="sh")"#.to_string(),
            vec![0]
        )]
    );
    assert_eq!(entries(&envelope)[0], refusal);
    let aborted = &entries(&envelope)[1];
    assert_eq!(aborted["tool"], "memory.remember");
    assert_eq!(aborted["aborted"], true);
    assert_eq!(
        envelope["summary"],
        json!({"total": 2, "succeeded": 0, "failed": 1, "aborted": 1})
    );
    assert_eq!(envelope["status"], "partial");
}

#[tokio::test]
async fn an_unknown_outcome_marks_every_sent_operation_and_aborts_the_rest() {
    let r = routing(&["exec", "git"]);
    let homes = FakeHomes::new(|home, request| match home {
        Home::Computer => HomeOutcome::Failed {
            error: json!({"kind": "computer_unreachable", "message": "connection reset"}),
            disposition: DomainDisposition::Unknown,
        },
        Home::Store => served(home, request),
    });
    let envelope = run(
        &r,
        r#"memory.recall(query="a") | exec.run(tool="sh") | git.log(limit=1) | memory.remember(content="x")"#,
        &homes,
    )
    .await;
    let sent: Vec<(Home, Vec<usize>)> = homes
        .requests()
        .into_iter()
        .map(|(home, _, indices)| (home, indices))
        .collect();
    assert_eq!(
        sent,
        vec![(Home::Store, vec![0]), (Home::Computer, vec![1, 2])]
    );
    for index in [1, 2] {
        let entry = &entries(&envelope)[index];
        assert_eq!(entry["ok"], false, "{entry}");
        assert_eq!(entry["error"]["kind"], "computer_unreachable", "{entry}");
        assert_eq!(entry["domain_disposition"], "unknown", "{entry}");
    }
    assert_eq!(entries(&envelope)[3]["aborted"], true);
    assert_eq!(
        envelope["summary"],
        json!({"total": 4, "succeeded": 1, "failed": 2, "aborted": 1})
    );
}

#[tokio::test]
async fn a_command_pack_the_computer_does_not_serve_is_refused_without_a_send() {
    let r = routing(&["computer"]);
    let homes = FakeHomes::new(served);
    let envelope = run(&r, r#"exec.run(tool="sh")"#, &homes).await;
    assert!(homes.requests().is_empty());
    let entry = &entries(&envelope)[0];
    assert_eq!(entry["error"]["kind"], "runs_on_computer", "{entry}");
    assert_eq!(entry["domain_disposition"], "not_committed", "{entry}");
    let message = entry["error"]["message"].as_str().expect("a message");
    assert!(
        message.contains("build-1 (127.0.0.1:8700)") && message.contains("exec pack"),
        "{message}"
    );

    let envelope = run(
        &r,
        r#"[exec.run(tool="sh"), memory.recall(query="a")]"#,
        &homes,
    )
    .await;
    assert_eq!(
        homes.requests(),
        vec![(
            Home::Store,
            r#"[memory.recall(query="a")]"#.to_string(),
            vec![1]
        )]
    );
    assert_eq!(entries(&envelope)[0]["error"]["kind"], "runs_on_computer");
    assert_eq!(entries(&envelope)[1]["ok"], true);
}

#[tokio::test]
async fn a_computer_answer_that_is_not_the_envelope_fails_with_an_unknown_outcome() {
    let r = routing(&["exec"]);
    let bad_answers = [
        json!({"results": []}),
        json!({"results": [{"ok": true, "tool": "git.log", "result": {}}]}),
        json!({"results": [{"tool": "exec.run"}]}),
        json!("text"),
    ];
    for bad in bad_answers {
        let answer = bad.clone();
        let homes = FakeHomes::new(move |home, request| match home {
            Home::Computer => HomeOutcome::Envelope(answer.clone()),
            Home::Store => served(home, request),
        });
        let envelope = run(
            &r,
            r#"memory.recall(query="a") | exec.run(tool="sh")"#,
            &homes,
        )
        .await;
        let entry = &entries(&envelope)[1];
        assert_eq!(entry["error"]["kind"], "computer_protocol", "{bad}");
        assert_eq!(entry["domain_disposition"], "unknown", "{bad}");
        let message = entry["error"]["message"].as_str().expect("a message");
        assert!(message.contains("build-1 (127.0.0.1:8700)"), "{message}");
    }
}

#[tokio::test]
async fn a_prev_path_the_earlier_segment_lacks_fails_before_any_send() {
    let r = routing(&["exec"]);
    let homes = FakeHomes::new(served);
    let envelope = run(
        &r,
        r#"get(id="n1") | exec.run(tree=$prev.missing) | exec.runs(session_id="s")"#,
        &homes,
    )
    .await;
    assert_eq!(homes.requests().len(), 1);
    let entry = &entries(&envelope)[1];
    assert_eq!(entry["error"]["kind"], "substitution_error", "{entry}");
    assert_eq!(entry["error"]["reason"], "path_not_found", "{entry}");
    assert_eq!(entries(&envelope)[2]["aborted"], true);
}

#[tokio::test]
async fn a_failed_store_operation_keeps_the_rest_of_the_chain_off_the_computer() {
    let r = routing(&["exec"]);
    let homes = FakeHomes::new(|home, request| match home {
        Home::Store => HomeOutcome::Envelope(json!({"results": [{
            "ok": false,
            "tool": "get",
            "error": {"kind": "not_found", "message": "no such id"},
            "domain_disposition": "not_committed",
        }]})),
        Home::Computer => served(home, request),
    });
    let envelope = run(&r, r#"get(id="n1") | exec.run(tree=$prev.tree)"#, &homes).await;
    assert!(homes
        .requests()
        .iter()
        .all(|(home, _, _)| *home == Home::Store));
    assert_eq!(entries(&envelope)[1]["aborted"], true);
}
