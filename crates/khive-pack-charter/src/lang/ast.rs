//! Parsed forms of charter and vocabulary sources, before registry resolution.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Advisory,
    Soft,
    Hard,
}

impl Level {
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "advisory" => Some(Self::Advisory),
            "soft" => Some(Self::Soft),
            "hard" => Some(Self::Hard),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Advisory => "advisory",
            Self::Soft => "soft",
            Self::Hard => "hard",
        }
    }

    /// Hard and soft requirements hold a phase until they pass; advisory ones are recorded only.
    pub const fn holds_phase(self) -> bool {
        !matches!(self, Self::Advisory)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Literal {
    String(String),
    Integer(i64),
    Duration(i64),
    Bool(bool),
    Word(String),
}

#[derive(Clone, Debug)]
pub struct Arg {
    pub name: Option<String>,
    pub value: Literal,
}

#[derive(Clone, Debug)]
pub struct Call {
    pub line: usize,
    pub name: String,
    pub args: Vec<Arg>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum End {
    Goto(String),
    Complete,
    Terminate(Option<String>),
}

#[derive(Clone, Debug)]
pub struct When {
    pub line: usize,
    pub call: Call,
    pub label: String,
    pub end: End,
}

#[derive(Clone, Debug)]
pub struct Require {
    pub call: Call,
    pub level: Level,
}

#[derive(Clone, Debug)]
pub struct ActionStep {
    pub call: Call,
    pub by: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Grant {
    pub line: usize,
    pub role: String,
    pub action: String,
}

#[derive(Clone, Debug)]
pub struct Phase {
    pub line: usize,
    pub id: String,
    pub owner: Option<String>,
    /// Each entry is one `await` line; the phase needs every line, and any one trigger of a line.
    pub awaits: Vec<(usize, Vec<String>)>,
    pub actions: Vec<ActionStep>,
    /// `perform <charter>` steps: each starts one child run and holds the phase until it ends.
    pub performs: Vec<(usize, String)>,
    pub requires: Vec<Require>,
    pub whens: Vec<When>,
    pub evidence: Vec<String>,
    pub grants: Vec<Grant>,
    pub max_rounds: Option<(usize, u32, String)>,
    pub end: Option<(usize, End)>,
}

#[derive(Clone, Debug)]
pub struct BreakGlass {
    pub line: usize,
    pub by: Option<String>,
    pub on: Vec<String>,
    pub reason_min: Option<i64>,
    pub marks_degraded: bool,
}

#[derive(Clone, Debug)]
pub struct CharterSource {
    pub id: String,
    pub version: i64,
    pub title: Option<String>,
    pub subject: Option<(usize, String)>,
    pub roles: Vec<(usize, String, Option<String>)>,
    pub triggers: Vec<(usize, String)>,
    pub phases: Vec<Phase>,
    pub break_glass: Option<BreakGlass>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EntryKind {
    Subject,
    Trigger,
    Phrase,
    Action,
}

impl EntryKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Subject => "subject",
            Self::Trigger => "trigger",
            Self::Phrase => "phrase",
            Self::Action => "action",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ParamSource {
    pub name: String,
    pub ty: String,
    pub optional: bool,
}

#[derive(Clone, Debug)]
pub struct EntrySource {
    pub line: usize,
    pub kind: EntryKind,
    pub name: String,
    pub description: Option<String>,
    pub outcomes: Option<Vec<String>>,
    pub passes: Option<Vec<String>>,
    pub ceiling: Option<Level>,
    pub params: Vec<ParamSource>,
}

#[derive(Clone, Debug)]
pub struct VocabularySource {
    pub name: String,
    pub version: i64,
    pub entries: Vec<EntrySource>,
}
