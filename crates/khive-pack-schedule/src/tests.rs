#[cfg(test)]
mod help_tests {
    use khive_types::Pack;

    use crate::pack::SchedulePack;

    fn find_handler(name: &str) -> &'static khive_types::HandlerDef {
        SchedulePack::HANDLERS
            .iter()
            .find(|h| h.name == name)
            .unwrap_or_else(|| panic!("handler {name:?} not found in schedule pack"))
    }

    #[test]
    fn remind_has_required_content_and_at() {
        let h = find_handler("schedule.remind");
        assert!(!h.params.is_empty(), "remind must have non-empty params");
        let content = h
            .params
            .iter()
            .find(|p| p.name == "content")
            .expect("remind must have 'content'");
        assert!(content.required, "remind.content must be required");
        let at = h
            .params
            .iter()
            .find(|p| p.name == "at")
            .expect("remind must have 'at'");
        assert!(at.required, "remind.at must be required");
    }

    #[test]
    fn remind_has_optional_repeat() {
        let h = find_handler("schedule.remind");
        let repeat = h
            .params
            .iter()
            .find(|p| p.name == "repeat")
            .expect("remind must have 'repeat'");
        assert!(!repeat.required, "remind.repeat must be optional");
    }

    #[test]
    fn schedule_has_required_action_and_at() {
        let h = find_handler("schedule.schedule");
        assert!(!h.params.is_empty(), "schedule must have non-empty params");
        let action = h
            .params
            .iter()
            .find(|p| p.name == "action")
            .expect("schedule must have 'action'");
        assert!(action.required, "schedule.action must be required");
        let at = h
            .params
            .iter()
            .find(|p| p.name == "at")
            .expect("schedule must have 'at'");
        assert!(at.required, "schedule.at must be required");
    }

    #[test]
    fn schedule_has_optional_repeat() {
        let h = find_handler("schedule.schedule");
        let repeat = h
            .params
            .iter()
            .find(|p| p.name == "repeat")
            .expect("schedule must have 'repeat'");
        assert!(!repeat.required, "schedule.repeat must be optional");
    }

    #[test]
    fn agenda_has_optional_from_to_limit() {
        let h = find_handler("schedule.agenda");
        assert!(!h.params.is_empty(), "agenda must have non-empty params");
        for name in ["from", "to", "limit"] {
            let p = h
                .params
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("agenda must have {name:?}"));
            assert!(!p.required, "agenda.{name} must be optional");
        }
    }

    #[test]
    fn cancel_has_required_id() {
        let h = find_handler("schedule.cancel");
        assert!(!h.params.is_empty(), "cancel must have non-empty params");
        let id = h
            .params
            .iter()
            .find(|p| p.name == "id")
            .expect("cancel must have 'id'");
        assert!(id.required, "cancel.id must be required");
    }

    #[test]
    fn all_schedule_handlers_have_non_empty_params() {
        for handler in SchedulePack::HANDLERS {
            assert!(
                !handler.params.is_empty(),
                "schedule handler {:?} must have non-empty params",
                handler.name
            );
        }
    }
}
