use khive_graph_diff::{
    diff, FieldChange, Fields, GraphDiff, GraphState, Id128, Record, SubstrateDiff,
};
use serde_json::{json, Value};

#[derive(Clone, Copy)]
pub enum Stream {
    Entity,
    Edge,
    Note,
}

impl Stream {
    pub const ALL: [Self; 3] = [Self::Entity, Self::Edge, Self::Note];

    pub fn name(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::Edge => "edge",
            Self::Note => "note",
        }
    }

    fn identity(self) -> &'static str {
        match self {
            Self::Edge => "edge_id",
            _ => "id",
        }
    }

    pub fn select(self, delta: &GraphDiff) -> &SubstrateDiff {
        match self {
            Self::Entity => &delta.entities,
            Self::Edge => &delta.edges,
            Self::Note => &delta.notes,
        }
    }
}

fn fields(stream: Stream, ordinal: usize, version: u64) -> Fields {
    [
        ("kind".to_string(), json!(stream.name())),
        ("label".to_string(), json!(format!("fixture-{ordinal:06}"))),
        (
            "payload".to_string(),
            json!({"items": [ordinal, "λ", null], "enabled": ordinal.is_multiple_of(2)}),
        ),
        ("version".to_string(), json!(version)),
    ]
    .into_iter()
    .collect()
}

fn lines(stream: Stream, count: usize, first_id: u128, version: u64) -> String {
    let mut text = String::with_capacity(count * 192);
    for ordinal in 0..count {
        let mut object: serde_json::Map<String, Value> =
            fields(stream, ordinal, version).into_iter().collect();
        object.insert(
            stream.identity().to_string(),
            json!(Id128::from_u128(first_id + ordinal as u128).to_string()),
        );
        text.push_str(&Value::Object(object).to_string());
        text.push('\n');
    }
    text
}

fn validate_records(
    records: &[Record],
    stream: Stream,
    count: usize,
    first_id: u128,
    version: u64,
) {
    assert_eq!(records.len(), count);
    for (ordinal, record) in records.iter().enumerate() {
        assert_eq!(record.id, Id128::from_u128(first_id + ordinal as u128));
        assert_eq!(record.fields, fields(stream, ordinal, version));
        assert_eq!(record.fields.len(), 4);
        assert!(!record.fields.contains_key(stream.identity()));
    }
}

pub struct ParseFixture {
    pub text: String,
    pub count: usize,
    pub stream: Stream,
}

impl ParseFixture {
    pub fn new(stream: Stream, count: usize) -> Self {
        assert!([1_000, 50_000, 500_000].contains(&count));
        Self {
            text: lines(stream, count, 1, 0),
            count,
            stream,
        }
    }

    pub fn parse(&self) -> GraphState {
        let result = match self.stream {
            Stream::Entity => GraphState::from_ndjson(&self.text, "", ""),
            Stream::Edge => GraphState::from_ndjson("", &self.text, ""),
            Stream::Note => GraphState::from_ndjson("", "", &self.text),
        };
        result.expect("valid generated NDJSON")
    }

    pub fn validate(&self, graph: &GraphState) {
        // Public inspection clones fields into the delta; it is outside every measurement.
        let delta = diff(&GraphState::default(), graph);
        for stream in Stream::ALL {
            let actual = stream.select(&delta);
            assert!(actual.removed.is_empty() && actual.modified.is_empty());
            if stream.name() == self.stream.name() {
                validate_records(&actual.added, stream, self.count, 1, 0);
            } else {
                assert!(actual.added.is_empty());
            }
        }
    }
}

#[derive(Clone, Copy)]
pub enum Relation {
    Matched,
    Modified,
    Disjoint,
}

impl Relation {
    pub const ALL: [Self; 3] = [Self::Matched, Self::Modified, Self::Disjoint];

    pub fn name(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::Modified => "modified",
            Self::Disjoint => "disjoint",
        }
    }
}

fn graph(count: usize, first_id: u128, version: u64) -> GraphState {
    let inputs = Stream::ALL.map(|stream| lines(stream, count, first_id, version));
    let graph = GraphState::from_ndjson(&inputs[0], &inputs[1], &inputs[2])
        .expect("three generated independent substrates");
    drop(inputs);
    let delta = diff(&GraphState::default(), &graph);
    for stream in Stream::ALL {
        let actual = stream.select(&delta);
        assert!(actual.removed.is_empty() && actual.modified.is_empty());
        validate_records(&actual.added, stream, count, first_id, version);
    }
    graph
}

pub struct DiffFixture {
    pub before: GraphState,
    pub after: GraphState,
    pub count_per_substrate: usize,
    pub relation: Relation,
}

impl DiffFixture {
    pub fn new(relation: Relation, count_per_substrate: usize) -> Self {
        assert!([1_000, 50_000].contains(&count_per_substrate));
        let (first_id, version) = match relation {
            Relation::Matched => (1, 0),
            Relation::Modified => (1, 1),
            Relation::Disjoint => (1_000_001, 0),
        };
        let result = Self {
            before: graph(count_per_substrate, 1, 0),
            after: graph(count_per_substrate, first_id, version),
            count_per_substrate,
            relation,
        };
        result.validate(&diff(&result.before, &result.after));
        result
    }

    pub fn validate(&self, delta: &GraphDiff) {
        for stream in Stream::ALL {
            let actual = stream.select(delta);
            match self.relation {
                Relation::Matched => assert!(actual.is_empty()),
                Relation::Modified => {
                    assert!(actual.added.is_empty() && actual.removed.is_empty());
                    assert_eq!(actual.modified.len(), self.count_per_substrate);
                    for (ordinal, record) in actual.modified.iter().enumerate() {
                        assert_eq!(record.id, Id128::from_u128(ordinal as u128 + 1));
                        assert_eq!(
                            record.changes,
                            vec![FieldChange {
                                field: "version".to_string(),
                                before: Some(json!(0)),
                                after: Some(json!(1)),
                            }]
                        );
                    }
                }
                Relation::Disjoint => {
                    assert!(actual.modified.is_empty());
                    validate_records(&actual.removed, stream, self.count_per_substrate, 1, 0);
                    validate_records(
                        &actual.added,
                        stream,
                        self.count_per_substrate,
                        1_000_001,
                        0,
                    );
                }
            }
        }
    }
}
