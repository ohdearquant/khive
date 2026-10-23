//! Request context — the who/where/when accompanying every gated request.

use khive_types::Namespace;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeLevel {
    Global,
    Namespace,
    Session,
    Task,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestContext {
    pub namespace: Namespace,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
}

impl RequestContext {
    pub fn new(namespace: Namespace) -> Self {
        Self {
            namespace,
            session_id: None,
            task_id: None,
        }
    }

    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_task(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    pub fn scope_level(&self) -> ScopeLevel {
        match (&self.session_id, &self.task_id) {
            (_, Some(_)) => ScopeLevel::Task,
            (Some(_), None) => ScopeLevel::Session,
            (None, None) => ScopeLevel::Namespace,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_context_preserves_scope_precedence() {
        let context = RequestContext::new(Namespace::parse("team:docs").unwrap());
        assert_eq!(context.namespace.as_str(), "team:docs");
        assert_eq!(context.scope_level(), ScopeLevel::Namespace);
        assert_eq!(
            context.clone().with_session("session-a").scope_level(),
            ScopeLevel::Session
        );
        assert_eq!(
            context.clone().with_task("task-a").scope_level(),
            ScopeLevel::Task
        );
        assert_eq!(
            context
                .with_session("session-a")
                .with_task("task-a")
                .scope_level(),
            ScopeLevel::Task
        );
    }
}
