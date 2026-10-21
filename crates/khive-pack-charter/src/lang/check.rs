//! Registry resolution and topology checks: a parsed charter becomes a canonical definition or
//! a list of refusals naming the refused construct.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde_json::{json, Value};

use super::ast::*;
use super::definition::*;
use super::registry::{EntryDef, ParamType, Registry, CHILD_RUN_OUTCOMES};
use super::{canonical_json, digest, Diagnostic};

#[derive(Clone, Debug)]
pub struct CompiledCharter {
    pub definition: Definition,
    pub definition_bytes: Vec<u8>,
    pub definition_digest: String,
    pub gate_registry_digest: String,
}

impl CompiledCharter {
    pub fn summary(&self) -> Value {
        json!({
            "id": self.definition.id,
            "version": self.definition.version,
            "subject": self.definition.subject,
            "phases": self.definition.phases.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            "performs": self.definition.phases.iter().flat_map(|p| p.performs.iter()).collect::<Vec<_>>(),
            "definition_digest": self.definition_digest,
            "gate_registry_digest": self.gate_registry_digest,
        })
    }
}

struct Checker<'a> {
    registry: &'a Registry,
    errors: Vec<Diagnostic>,
    referenced: BTreeMap<String, Value>,
    roles: BTreeSet<String>,
}

impl Checker<'_> {
    fn refuse(&mut self, line: usize, code: &'static str, message: impl Into<String>) {
        self.errors.push(Diagnostic::new(line, code, message));
    }

    fn lookup(&mut self, line: usize, name: &str, want: EntryKind) -> Option<EntryDef> {
        let registry = self.registry;
        match registry.get(name) {
            Some(entry) if entry.kind == want => {
                self.referenced.insert(entry.name.clone(), entry.to_json());
                Some(entry.clone())
            }
            Some(entry) => {
                self.refuse(
                    line,
                    "wrong_entry_kind",
                    format!(
                        "{name:?} is a registered {}, not a {}",
                        entry.kind.as_str(),
                        want.as_str()
                    ),
                );
                None
            }
            None => {
                let (code, what) = match want {
                    EntryKind::Subject => ("unknown_subject", "subject kind"),
                    EntryKind::Trigger => ("unknown_trigger", "trigger"),
                    EntryKind::Phrase => ("unknown_phrase", "phrase"),
                    EntryKind::Action => ("unknown_action", "action"),
                };
                let mut known = registry.names_of(&want);
                known.truncate(40);
                self.refuse(
                    line,
                    code,
                    format!(
                        "{name:?} is not a registered {what}; registered: {}",
                        known.join(", ")
                    ),
                );
                None
            }
        }
    }

    fn role(&mut self, line: usize, role: &str) {
        if !self.roles.contains(role) {
            self.refuse(
                line,
                "unknown_role",
                format!(
                    "role {role:?} is not declared in `roles:`; declared: {}",
                    self.roles.iter().cloned().collect::<Vec<_>>().join(", ")
                ),
            );
        }
    }

    fn args(&mut self, call: &Call, entry: &EntryDef) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        let positional = call.args.iter().filter(|a| a.name.is_none()).count();
        if positional > 0 && (entry.params.len() != 1 || call.args.len() != 1) {
            self.refuse(
                call.line,
                "positional_argument",
                format!(
                    "{:?} takes named arguments ({}); a positional argument is accepted only by a \
                     phrase with exactly one parameter",
                    entry.name,
                    entry
                        .params
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            return out;
        }
        for arg in &call.args {
            let name = arg
                .name
                .clone()
                .unwrap_or_else(|| entry.params[0].name.clone());
            let Some(param) = entry.params.iter().find(|p| p.name == name) else {
                self.refuse(
                    call.line,
                    "unknown_argument",
                    format!(
                        "{:?} has no parameter {name:?}; parameters: {}",
                        entry.name,
                        if entry.params.is_empty() {
                            "none".to_owned()
                        } else {
                            entry
                                .params
                                .iter()
                                .map(|p| format!("{} {}", p.name, p.ty.as_str()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    ),
                );
                continue;
            };
            let value = match (param.ty, &arg.value) {
                (ParamType::String, Literal::String(s)) => Value::String(s.clone()),
                (ParamType::Word, Literal::Word(w)) => Value::String(w.clone()),
                (ParamType::Integer, Literal::Integer(n)) => json!(n),
                (ParamType::Duration, Literal::Duration(n)) => json!(n),
                (ParamType::Bool, Literal::Bool(b)) => json!(b),
                (ty, _) => {
                    self.refuse(
                        call.line,
                        "argument_type",
                        format!(
                            "argument {name:?} of {:?} takes a {}",
                            entry.name,
                            ty.as_str()
                        ),
                    );
                    continue;
                }
            };
            if out.insert(name.clone(), value).is_some() {
                self.refuse(
                    call.line,
                    "duplicate_argument",
                    format!("argument {name:?} is given twice"),
                );
            }
        }
        for param in &entry.params {
            if !param.optional && !out.contains_key(&param.name) {
                self.refuse(
                    call.line,
                    "missing_argument",
                    format!("{:?} requires argument {:?}", entry.name, param.name),
                );
            }
        }
        out
    }
}

fn route(end: &End) -> Route {
    match end {
        End::Goto(phase) => Route::Goto {
            phase: phase.clone(),
        },
        End::Complete => Route::Complete,
        End::Terminate(reason) => Route::Terminate {
            reason: reason.clone(),
        },
    }
}

pub fn check(
    charter: &CharterSource,
    registry: &Registry,
) -> Result<CompiledCharter, Vec<Diagnostic>> {
    let mut c = Checker {
        registry,
        errors: Vec::new(),
        referenced: BTreeMap::new(),
        roles: BTreeSet::new(),
    };

    let subject = match &charter.subject {
        Some((line, kind)) => {
            c.lookup(*line, kind, EntryKind::Subject);
            kind.clone()
        }
        None => {
            c.refuse(
                1,
                "missing_subject",
                "a charter names its subject kind: `subject <kind>`",
            );
            String::new()
        }
    };

    let mut roles = Vec::new();
    for (line, name, description) in &charter.roles {
        if !c.roles.insert(name.clone()) {
            c.refuse(
                *line,
                "duplicate_role",
                format!("role {name:?} is declared twice"),
            );
        }
        roles.push(RoleDef {
            name: name.clone(),
            description: description.clone(),
        });
    }

    let mut triggers = Vec::new();
    for (line, name) in &charter.triggers {
        c.lookup(*line, name, EntryKind::Trigger);
        if triggers.contains(name) {
            c.refuse(
                *line,
                "duplicate_trigger",
                format!("trigger {name:?} is listed twice"),
            );
        }
        triggers.push(name.clone());
    }

    let mut ids: BTreeMap<String, usize> = BTreeMap::new();
    for phase in &charter.phases {
        if let Some(first) = ids.get(&phase.id) {
            c.refuse(
                phase.line,
                "duplicate_phase",
                format!("phase {:?} is already declared on line {first}", phase.id),
            );
        } else {
            ids.insert(phase.id.clone(), phase.line);
        }
    }
    let goto_target = |c: &mut Checker, line: usize, target: &str| {
        if !ids.contains_key(target) {
            c.refuse(
                line,
                "unknown_phase",
                format!(
                    "no phase {target:?}; phases: {}",
                    ids.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            );
        }
    };

    let mut phases = Vec::new();
    for (index, phase) in charter.phases.iter().enumerate() {
        if let Some(owner) = &phase.owner {
            c.role(phase.line, owner);
        }

        let mut awaits = Vec::new();
        for (line, line_triggers) in &phase.awaits {
            for name in line_triggers {
                c.lookup(*line, name, EntryKind::Trigger);
                if !triggers.is_empty() && !triggers.contains(name) {
                    c.refuse(
                        *line,
                        "undeclared_trigger",
                        format!("trigger {name:?} is awaited but not listed in `triggers:`"),
                    );
                }
            }
            awaits.push(line_triggers.clone());
        }

        let mut actions = Vec::new();
        for step in &phase.actions {
            if let Some(by) = &step.by {
                c.role(step.call.line, by);
            }
            if let Some(entry) = c.lookup(step.call.line, &step.call.name, EntryKind::Action) {
                let args = c.args(&step.call, &entry);
                actions.push(ActionDef {
                    name: entry.name.clone(),
                    args,
                    by: step.by.clone(),
                    passes: entry.passes.clone(),
                });
            }
        }

        let mut performs = Vec::new();
        for (line, child) in &phase.performs {
            if child == &charter.id {
                c.refuse(
                    *line,
                    "perform_self",
                    format!("charter {child:?} cannot perform itself"),
                );
            }
            if performs.contains(child) {
                c.refuse(
                    *line,
                    "duplicate_perform",
                    format!("phase {:?} performs {child:?} twice", phase.id),
                );
            }
            performs.push(child.clone());
        }

        let mut requires = Vec::new();
        for req in &phase.requires {
            let Some(entry) = c.lookup(req.call.line, &req.call.name, EntryKind::Phrase) else {
                continue;
            };
            let args = c.args(&req.call, &entry);
            if req.level > entry.ceiling {
                c.refuse(
                    req.call.line,
                    "level_above_ceiling",
                    format!(
                        "{:?} is required {} but its ceiling is {}: its truth cannot be \
                         established independently of the actor it checks",
                        entry.name,
                        req.level.as_str(),
                        entry.ceiling.as_str()
                    ),
                );
            }
            if entry.passes.is_empty() {
                c.refuse(
                    req.call.line,
                    "no_passing_outcome",
                    format!(
                        "{:?} has no passing outcome, so it cannot be required; route on it with \
                         `when {} is <label>` ({})",
                        entry.name,
                        entry.name,
                        entry.outcomes.join(", ")
                    ),
                );
            }
            requires.push(RequireDef {
                phrase: entry.name.clone(),
                args,
                level: req.level,
                passes: entry.passes.clone(),
            });
        }

        let mut whens = Vec::new();
        for when in &phase.whens {
            let name = &when.call.name;
            let (source, outcomes, args) = if performs.contains(name) {
                if !when.call.args.is_empty() {
                    c.refuse(
                        when.line,
                        "unknown_argument",
                        format!("a performed charter {name:?} takes no arguments"),
                    );
                }
                (
                    WhenSource::Charter,
                    CHILD_RUN_OUTCOMES.iter().map(|s| s.to_string()).collect(),
                    BTreeMap::new(),
                )
            } else {
                match registry.get(name).cloned() {
                    Some(entry) if entry.kind == EntryKind::Subject => {
                        c.refuse(
                            when.line,
                            "wrong_entry_kind",
                            format!("{name:?} is a subject kind; `when` routes on an outcome"),
                        );
                        continue;
                    }
                    Some(entry) => {
                        if entry.kind == EntryKind::Action
                            && !phase.actions.iter().any(|a| &a.call.name == name)
                        {
                            c.refuse(
                                when.line,
                                "action_not_in_phase",
                                format!(
                                    "`when {name} is ...` routes on an action this phase does not \
                                     take"
                                ),
                            );
                        }
                        c.referenced.insert(entry.name.clone(), entry.to_json());
                        let args = c.args(&when.call, &entry);
                        let source = match entry.kind {
                            EntryKind::Trigger => WhenSource::Trigger,
                            EntryKind::Action => WhenSource::Action,
                            _ => WhenSource::Phrase,
                        };
                        (source, entry.outcomes.clone(), args)
                    }
                    None => {
                        c.refuse(
                            when.line,
                            "unknown_phrase",
                            format!(
                                "{name:?} is neither a registered phrase, trigger or action nor a \
                                 charter this phase performs"
                            ),
                        );
                        continue;
                    }
                }
            };
            if !outcomes.contains(&when.label) {
                c.refuse(
                    when.line,
                    "unknown_label",
                    format!(
                        "{:?} is not an outcome of {name:?}; outcomes: {}",
                        when.label,
                        outcomes.join(", ")
                    ),
                );
            }
            if let End::Goto(target) = &when.end {
                goto_target(&mut c, when.line, target);
            }
            whens.push(WhenDef {
                name: name.clone(),
                source,
                args,
                label: when.label.clone(),
                route: route(&when.end),
            });
        }

        let mut grants = Vec::new();
        for grant in &phase.grants {
            c.role(grant.line, &grant.role);
            c.lookup(grant.line, &grant.action, EntryKind::Action);
            grants.push(GrantDef {
                role: grant.role.clone(),
                action: grant.action.clone(),
            });
        }

        let max_rounds = phase.max_rounds.as_ref().map(|(line, rounds, then)| {
            goto_target(&mut c, *line, then);
            if then == &phase.id {
                c.refuse(
                    *line,
                    "bad_max_rounds",
                    "max_rounds routes to a different phase once the bound is reached",
                );
            }
            MaxRounds {
                rounds: *rounds,
                then: then.clone(),
            }
        });

        let next = match &phase.end {
            Some((line, end)) => {
                if let End::Goto(target) = end {
                    goto_target(&mut c, *line, target);
                }
                route(end)
            }
            None => match charter.phases.get(index + 1) {
                Some(following) => Route::Goto {
                    phase: following.id.clone(),
                },
                None => {
                    c.refuse(
                        phase.line,
                        "open_end",
                        format!(
                            "phase {:?} is the last phase; it ends with complete, terminate or \
                             goto <phase>",
                            phase.id
                        ),
                    );
                    Route::Complete
                }
            },
        };

        phases.push(PhaseDef {
            id: phase.id.clone(),
            ordinal: index as i64 + 1,
            owner: phase.owner.clone(),
            awaits,
            actions,
            performs,
            requires,
            whens,
            evidence: phase.evidence.clone(),
            grants,
            max_rounds,
            next,
        });
    }

    let break_glass = charter.break_glass.as_ref().map(|bg| {
        if let Some(by) = &bg.by {
            c.role(bg.line, by);
        } else {
            c.refuse(
                bg.line,
                "bad_break_glass",
                "break_glass names its grantor role: `by <role>`",
            );
        }
        for target in &bg.on {
            let (phase_id, phrase) = target.split_once('.').unwrap_or((target, ""));
            let found = phases
                .iter()
                .find(|p| p.id == phase_id)
                .is_some_and(|p| p.requires.iter().any(|r| r.phrase == phrase));
            if !found {
                c.refuse(
                    bg.line,
                    "bad_break_glass",
                    format!("`on {target}` names no `require {phrase}` in phase {phase_id:?}"),
                );
            }
        }
        BreakGlassDef {
            by: bg.by.clone(),
            on: bg.on.clone(),
            reason_min: bg.reason_min,
            marks_degraded: bg.marks_degraded,
        }
    });

    if c.errors.is_empty() {
        check_topology(&charter.phases, &phases, &mut c.errors);
    }
    if !c.errors.is_empty() {
        c.errors.sort_by_key(|d| d.line);
        return Err(c.errors);
    }

    let registry_value = Value::Object(c.referenced.clone().into_iter().collect());
    let gate_registry_digest = digest(&canonical_json(&registry_value));
    let definition = Definition {
        language: LANGUAGE.to_owned(),
        id: charter.id.clone(),
        version: charter.version,
        title: charter.title.clone(),
        subject,
        roles,
        triggers,
        phases,
        break_glass,
        registry: c.referenced,
        gate_registry_digest: gate_registry_digest.clone(),
    };
    let value = serde_json::to_value(&definition).map_err(|e| {
        vec![Diagnostic::new(
            1,
            "internal",
            format!("definition encoding: {e}"),
        )]
    })?;
    let definition_bytes = canonical_json(&value);
    Ok(CompiledCharter {
        definition_digest: digest(&definition_bytes),
        definition,
        definition_bytes,
        gate_registry_digest,
    })
}

fn successors(phase: &PhaseDef) -> (Vec<&str>, bool) {
    let mut out = Vec::new();
    let mut terminal = false;
    for r in phase.whens.iter().map(|w| &w.route).chain([&phase.next]) {
        match r {
            Route::Goto { phase } => out.push(phase.as_str()),
            Route::Complete | Route::Terminate { .. } => terminal = true,
        }
    }
    if let Some(bound) = &phase.max_rounds {
        out.push(bound.then.as_str());
    }
    (out, terminal)
}

/// Every phase is reachable from the first, every cycle passes a phase bounded by
/// `max_rounds`, and from every phase `complete` or `terminate` is reachable.
fn check_topology(sources: &[Phase], phases: &[PhaseDef], errors: &mut Vec<Diagnostic>) {
    if phases.is_empty() {
        errors.push(Diagnostic::new(
            1,
            "empty_workflow",
            "a workflow declares at least one phase",
        ));
        return;
    }
    let index: BTreeMap<&str, usize> = phases
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id.as_str(), i))
        .collect();
    let edges: Vec<(Vec<usize>, bool)> = phases
        .iter()
        .map(|p| {
            let (next, terminal) = successors(p);
            (
                next.iter()
                    .filter_map(|id| index.get(id).copied())
                    .collect(),
                terminal,
            )
        })
        .collect();

    let mut reached = vec![false; phases.len()];
    let mut queue = VecDeque::from([0usize]);
    reached[0] = true;
    while let Some(i) = queue.pop_front() {
        for &j in &edges[i].0 {
            if !reached[j] {
                reached[j] = true;
                queue.push_back(j);
            }
        }
    }
    for (i, phase) in phases.iter().enumerate() {
        if !reached[i] {
            errors.push(Diagnostic::new(
                sources[i].line,
                "unreachable_phase",
                format!(
                    "phase {:?} is not reachable from {:?}",
                    phase.id, phases[0].id
                ),
            ));
        }
    }

    // A phase carrying max_rounds bounds every cycle through it, so cycles are searched in the
    // graph without those phases.
    let bounded: Vec<bool> = phases.iter().map(|p| p.max_rounds.is_some()).collect();
    let mut state = vec![0u8; phases.len()];
    let mut stack: Vec<usize> = Vec::new();
    fn visit(
        i: usize,
        edges: &[(Vec<usize>, bool)],
        bounded: &[bool],
        state: &mut [u8],
        stack: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        state[i] = 1;
        stack.push(i);
        for &j in &edges[i].0 {
            if bounded[j] {
                continue;
            }
            if state[j] == 1 {
                let start = stack.iter().position(|&k| k == j).unwrap_or(0);
                return Some(stack[start..].to_vec());
            }
            if state[j] == 0 {
                if let Some(cycle) = visit(j, edges, bounded, state, stack) {
                    return Some(cycle);
                }
            }
        }
        stack.pop();
        state[i] = 2;
        None
    }
    for i in 0..phases.len() {
        if bounded[i] || state[i] != 0 {
            continue;
        }
        if let Some(cycle) = visit(i, &edges, &bounded, &mut state, &mut stack) {
            let names: Vec<&str> = cycle.iter().map(|&k| phases[k].id.as_str()).collect();
            errors.push(Diagnostic::new(
                sources[cycle[0]].line,
                "unbounded_cycle",
                format!(
                    "phases {} form a cycle with no `max_rounds N then <phase>` bound",
                    names.join(" -> ")
                ),
            ));
            break;
        }
    }

    let mut exits = vec![false; phases.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..phases.len() {
            if !exits[i] && (edges[i].1 || edges[i].0.iter().any(|&j| exits[j])) {
                exits[i] = true;
                changed = true;
            }
        }
    }
    for (i, phase) in phases.iter().enumerate() {
        if reached[i] && !exits[i] {
            errors.push(Diagnostic::new(
                sources[i].line,
                "no_exit",
                format!(
                    "neither complete nor terminate is reachable from phase {:?}",
                    phase.id
                ),
            ));
        }
    }
}
