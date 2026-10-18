use std::collections::BTreeMap;

use khive_request::{
    ArgValue, AtomicRejection, ExecutionMode, ParsedOp, ParsedRequest, TypedJsonOp,
};
use khive_types::pack::AtomicRejectionReason;
use serde_json::{json, Value};

pub const SINGLE_SIMPLE: &str = r#"verb(arg="value")"#;

pub const SINGLE_COMPLEX: &str = r#"memory.remember(content="Long content string that represents a real-world memory payload", salience=0.85, decay_factor=0.0, memory_type="semantic", source_id="550e8400-e29b-41d4-a716-446655440000")"#;

pub const BATCH_3: &str = r#"[create(kind="concept", name="LoRA"), search(kind="entity", query="attention"), get(id="550e8400-e29b-41d4-a716-446655440000")]"#;

pub fn make_batch(n: usize) -> String {
    let ops: Vec<String> = (0..n)
        .map(|i| format!(r#"create(kind="concept", name="concept_{i}", description="desc_{i}")"#))
        .collect();
    format!("[{}]", ops.join(", "))
}

pub const CHAIN_2: &str = r#"create(kind="concept", name="LoRA") | link(source_id=$prev.id, target_id="550e8400-e29b-41d4-a716-446655440000", relation="extends")"#;

pub fn make_chain(n: usize) -> String {
    let mut ops: Vec<String> = vec![r#"create(kind="concept", name="seed")"#.to_owned()];
    for i in 1..n {
        ops.push(format!(
            r#"create(kind="concept", name="step_{i}", description=$prev.id)"#
        ));
    }
    ops.join(" | ")
}

pub fn expected_batch(count: usize) -> ParsedRequest {
    ParsedRequest {
        ops: (0..count)
            .map(|i| ParsedOp {
                tool: "create".into(),
                args: [
                    ("kind".into(), ArgValue::Value(json!("concept"))),
                    (
                        "name".into(),
                        ArgValue::Value(json!(format!("concept_{i}"))),
                    ),
                    (
                        "description".into(),
                        ArgValue::Value(json!(format!("desc_{i}"))),
                    ),
                ]
                .into_iter()
                .collect(),
            })
            .collect(),
        mode: ExecutionMode::Parallel,
        ranges: (0..count).map(|i| i..i + 1).collect(),
    }
}

pub fn expected_chain(count: usize) -> ParsedRequest {
    let ops = (0..count)
        .map(|i| {
            let mut args = BTreeMap::from([
                ("kind".into(), ArgValue::Value(json!("concept"))),
                (
                    "name".into(),
                    ArgValue::Value(json!(if i == 0 {
                        "seed".into()
                    } else {
                        format!("step_{i}")
                    })),
                ),
            ]);
            if i > 0 {
                args.insert(
                    "description".into(),
                    ArgValue::PrevRef { path: "id".into() },
                );
            }
            ParsedOp {
                tool: "create".into(),
                args,
            }
        })
        .collect();
    ParsedRequest {
        ops,
        mode: ExecutionMode::Chain,
        ranges: std::iter::once(0..count).collect(),
    }
}

pub fn typed_batch(count: usize) -> (Vec<TypedJsonOp>, ParsedRequest) {
    let typed: Vec<_> = (0..count).map(|i| {
        let Value::Object(args) = json!({"kind":"concept", "name":format!("typed_{i}"),
            "properties":{"index":i, "flags":[true,false,null], "nested":{"label":format!("value_{i}")}}})
            else { unreachable!("fixture object") };
        TypedJsonOp { tool: "create".into(), args }
    }).collect();
    let expected = ParsedRequest {
        ops: typed
            .iter()
            .map(|op| ParsedOp {
                tool: op.tool.clone(),
                args: op
                    .args
                    .iter()
                    .map(|(key, value)| (key.clone(), ArgValue::Value(value.clone())))
                    .collect(),
            })
            .collect(),
        mode: ExecutionMode::Parallel,
        ranges: (0..count).map(|i| i..i + 1).collect(),
    };
    (typed, expected)
}

pub fn atomic_batch(class: &str, count: usize) -> (Vec<ParsedOp>, Vec<AtomicRejection>) {
    use AtomicRejectionReason::{EmbeddingBearing, KnownUnimplemented, Read, Unlisted};
    let pattern: &[(&str, Option<AtomicRejectionReason>)] = match class {
        "eligible" => &[("update", None), ("delete", None), ("link", None)],
        "mixed" => &[
            ("update", None),
            ("create", Some(EmbeddingBearing)),
            ("delete", None),
            ("search", Some(Read)),
            ("missing.benchmark", Some(Unlisted)),
            ("merge", Some(KnownUnimplemented)),
        ],
        "rejected" => &[
            ("create", Some(EmbeddingBearing)),
            ("search", Some(Read)),
            ("missing.benchmark", Some(Unlisted)),
            ("merge", Some(KnownUnimplemented)),
        ],
        _ => panic!("unknown fixture class"),
    };
    let mut ops = Vec::with_capacity(count);
    let mut expected = Vec::new();
    for i in 0..count {
        let (tool, reason) = pattern[i % pattern.len()];
        ops.push(ParsedOp {
            tool: tool.into(),
            args: BTreeMap::new(),
        });
        if let Some(reason) = reason {
            expected.push(AtomicRejection {
                op_index: i,
                tool: tool.into(),
                reason,
            });
        }
    }
    (ops, expected)
}

pub fn catalog() -> BTreeMap<String, String> {
    [
        ("create", "kg"),
        ("search", "kg"),
        ("get", "kg"),
        ("link", "kg"),
        ("memory.remember", "memory"),
    ]
    .into_iter()
    .map(|(verb, pack)| (verb.into(), pack.into()))
    .collect()
}

pub fn check_plan(plan: &Value, parsed: &ParsedRequest, catalog: &BTreeMap<String, String>) {
    assert_eq!(plan["parsed"], true);
    assert!(plan.get("error").is_none());
    assert_eq!(plan["stage_count"], parsed.ops.len());
    let mode = match parsed.mode {
        ExecutionMode::Single => "single",
        ExecutionMode::Parallel => "parallel",
        ExecutionMode::Chain => "chain",
    };
    assert_eq!(plan["mode"], mode);
    assert_eq!(plan["limits"]["max_ops"], khive_request::MAX_OPS);
    let units = plan["units"].as_array().expect("plan units");
    assert_eq!(units.len(), parsed.ranges.len());
    for (unit, range) in units.iter().zip(&parsed.ranges) {
        assert_eq!(unit["start"], range.start);
        assert_eq!(unit["end"], range.end);
    }
    let stages = plan["stages"].as_array().expect("plan stages");
    assert_eq!(stages.len(), parsed.ops.len());
    for (index, (stage, op)) in stages.iter().zip(&parsed.ops).enumerate() {
        assert_eq!(stage["index"], index);
        assert_eq!(stage["verb"], op.tool);
        assert_eq!(stage["known"], catalog.contains_key(&op.tool));
        assert_eq!(stage["pack"], json!(catalog.get(&op.tool)));
        let unit = parsed
            .ranges
            .iter()
            .position(|range| range.contains(&index))
            .unwrap();
        assert_eq!(stage["unit_index"], unit);
        assert_eq!(stage["args"].as_object().unwrap().len(), op.args.len());
        let mut references = Vec::new();
        for (name, arg) in &op.args {
            match arg {
                ArgValue::Value(value) => assert_eq!(&stage["args"][name], value),
                ArgValue::PrevRef { path } => {
                    assert_eq!(path, "id", "legacy fixture path");
                    assert_eq!(stage["args"][name], "$prev.id");
                    references.push(path.as_str());
                }
                _ => panic!("legacy fixture unexpectedly contains nested references"),
            }
        }
        assert_eq!(stage["prev_refs"], json!(references));
    }
}
