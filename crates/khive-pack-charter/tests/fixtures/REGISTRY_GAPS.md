# Phrase registry gaps for workflow fixtures

These candidate phrases are intentionally absent from the fixtures and vocabularies. Their
outcomes and ceilings describe the evidence needed before they can be registered.

| Phrase | Observation kind | Arguments | Outcomes | Proposed ceiling | Producer class | Fixtures |
| --- | --- | --- | --- | --- | --- | --- |
| `task.triage_disposition` | `task.triage` | `task:word` | `actionable`, `waiting`, `moot`, `complete`, `uncertain` | hard | task-store reader | `stale_task_triage` |
| `task.transition_reason` | `task.transition` | `task:word` | `recorded`, `missing` | hard | task-store reader | `stale_task_triage` |
| `task.external_gate_live` | `task.external_gate` | `task:word` | `live`, `cleared`, `unknown` | hard | task-store and condition reader | `stale_task_triage` |
| `task.owner_notified` | `task.owner_notification` | `task:word`, `owner:word` | `delivered`, `failed`, `not_required` | advisory | message delivery reader | `stale_task_triage` |
| `investigation.read_authorized` | `investigation.authorization` | `scope:word` | `allowed`, `refused`, `unknown` | advisory | requester or policy record | `issue_readonly_investigation` |
| `investigation.source_read` | `source.read` | `source:word` | `read`, `unreadable`, `absent` | hard | source reader | `issue_readonly_investigation`, `code_fix_with_probe_test` |
| `investigation.state_current` | `issue.state` | `issue:word` | `current`, `stale`, `absent`, `unavailable` | hard | issue tracker reader | `issue_readonly_investigation` |
| `investigation.prior_change_checked` | `issue.prior_change` | `issue:word` | `not_landed`, `already_landed`, `unavailable` | hard | forge reader | `issue_readonly_investigation` |
| `investigation.probe_result` | `investigation.probe` | `probe:word` | `pass`, `fail`, `blocked` | hard | command runner | `issue_readonly_investigation` |
| `investigation.write_scope` | `workspace.write_scope` | `scope:word` | `within_scope`, `unexpected_write`, `unreadable` | hard | workspace reader | `issue_readonly_investigation` |
| `report.delivery` | `report.delivery` | `channel:word` | `delivered`, `failed`, `pending` | advisory | reply or message delivery reader | `stale_task_triage`, `issue_readonly_investigation`, `code_fix_with_probe_test`, `pr_merge_after_ci_gate` |
| `change.premise_verified` | `change.premise` | `source:word` | `verified`, `refuted`, `unavailable` | hard | source reader | `code_fix_with_probe_test` |
| `change.red_probe` | `change.red_probe` | `test:word` | `reproduced`, `not_reproduced`, `blocked` | hard | test runner | `code_fix_with_probe_test` |
| `change.tests_green` | `change.test_run` | `suite:word` | `pass`, `fail`, `pending` | hard | test runner | `code_fix_with_probe_test` |
| `change.lint_clean` | `change.lint_run` | `tool:word` | `pass`, `fail`, `pending` | hard | lint runner | `code_fix_with_probe_test` |
| `change.mutation_caught` | `change.mutation_run` | `probe:word` | `caught`, `escaped`, `blocked` | hard | mutation harness | `code_fix_with_probe_test` |
| `change.publication_scan_clear` | `change.publication_scan` | `scope:word` | `clear`, `finding`, `unavailable` | hard | repository scanner | `code_fix_with_probe_test` |
| `change.publish_head_verified` | `change.published_head` | `expected_head:word` | `match`, `mismatch`, `unavailable` | hard | forge reader | `code_fix_with_probe_test` |
| `change.live_behavior_verified` | `change.live_probe` | `service:word` | `pass`, `fail`, `blocked` | hard | runtime probe | `code_fix_with_probe_test` |
| `pull_request.mergeable` | `pull_request.mergeability` | `head:word` | `clean`, `blocked`, `unknown` | hard | forge reader | `pr_merge_after_ci_gate` |
| `pull_request.merge_tree_verified` | `pull_request.merge_tree` | `head:word`, `base:word` | `clean`, `conflict`, `unknown` | hard | repository reader | `pr_merge_after_ci_gate` |
| `pull_request.merge_gate_passed` | `pull_request.merge_gate` | `head:word` | `pass`, `fail`, `unknown` | hard | merge gate runner | `pr_merge_after_ci_gate` |
| `pull_request.main_tree_verified` | `pull_request.main_tree` | `expected_tree:word` | `matches`, `mismatch`, `unknown` | hard | repository reader | `pr_merge_after_ci_gate` |
| `pull_request.postmerge_state` | `pull_request.merge_state` | `pull_request:word` | `merged`, `not_merged`, `unknown` | hard | forge reader | `pr_merge_after_ci_gate` |
| `inbox.identity_verified` | `actor.identity` | `actor:word` | `match`, `mismatch`, `unavailable` | hard | identity reader | `inbox_sweep` |
| `inbox.live_state_verified` | `source.state` | `source:word`, `revision:word` | `current`, `stale`, `unavailable` | hard | source reader | `inbox_sweep` |
| `inbox.thread_read_complete` | `message.thread` | `message:word` | `complete`, `partial`, `unreadable` | hard | thread reader | `inbox_sweep` |
| `inbox.message_marked_read` | `inbox.mark_read` | `message:word` | `marked`, `failed`, `unavailable` | advisory | mailbox state reader | `inbox_sweep` |
| `inbox.reply_delivered` | `message.delivery` | `message:word` | `delivered`, `failed`, `pending` | advisory | message delivery reader | `inbox_sweep` |
| `inbox.sweep_recorded` | `session.log` | `sweep:word` | `written`, `failed`, `unavailable` | advisory | session log reader | `inbox_sweep` |
| `inbox.monitor_armed` | `inbox.monitor` | `interval:duration` | `armed`, `absent`, `refused` | advisory | monitor scheduler | `inbox_sweep` |
| `inbox.next_wake_scheduled` | `schedule.next_wake` | `pending_work:boolean` | `scheduled`, `absent`, `refused` | advisory | schedule reader | `inbox_sweep` |
| `merge.ask_head_exact` | `pull_request.ask_head` | `pull_request:word`, `head:word` | `match`, `mismatch`, `unavailable` | hard | diff and message reader | `pr_merge_decision_handoff` |
| `merge.ask_delivered` | `message.delivery` | `pull_request:word`, `head:word` | `delivered`, `failed`, `pending` | advisory | message delivery reader | `pr_merge_decision_handoff` |
| `merge.maintainer_authorization` | `pull_request.authorization` | `pull_request:word`, `head:word` | `granted`, `refused`, `pending` | hard | maintainer decision record | `pr_merge_decision_handoff` |
| `merge.named_contexts_at_head` | `check.context_set` | `event:word`, `head:word` | `pass`, `fail`, `pending`, `unknown` | hard | forge status reader | `pr_merge_decision_handoff` |
| `review.finding_verified` | `review.finding` | `finding:word`, `head:word` | `reproduced`, `refuted`, `unavailable` | hard | source execution reader | `pr_review_verdict_escalation` |
| `review.head_gate_recorded` | `review.head_gate` | `head:word` | `pass`, `fail`, `pending`, `unavailable` | hard | head gate reader | `pr_review_verdict_escalation` |
| `review.merge_authority` | `pull_request.merge_authority` | `pull_request:word`, `head:word` | `maintainer_only`, `granted`, `refused` | hard | authority record reader | `pr_review_verdict_escalation` |
| `directive.target_state_verified` | `source.state` | `target:word`, `revision:word` | `current`, `stale`, `unavailable` | hard | source reader | `directive_relay` |
| `directive.target_owner_matches` | `workstream.routing` | `workstream:word`, `owner:word` | `match`, `mismatch`, `unavailable` | advisory | routing table reader | `directive_relay` |
| `directive.relay_delivered` | `message.delivery` | `target:word`, `directive:word` | `delivered`, `failed`, `pending` | advisory | message delivery reader | `directive_relay` |
| `directive.source_attributed` | `message.source` | `message:word` | `attached`, `missing`, `unavailable` | advisory | message source reader | `directive_relay` |
| `directive.secret_scan_clear` | `message.secret_scan` | `message:word` | `clear`, `found`, `unavailable` | hard | secret scanner | `directive_relay` |
| `directive.outbound_policy_passed` | `message.outbound_policy` | `message:word` | `pass`, `blocked`, `unavailable` | advisory | outbound policy reader | `directive_relay` |
| `directive.human_report_delivered` | `message.report` | `directive:word` | `delivered`, `failed`, `pending` | advisory | message delivery reader | `directive_relay` |
| `gate.package_hash_matches_head` | `gate.package_hash` | `package:word`, `head:word` | `match`, `mismatch`, `unavailable` | hard | package hash reader | `pr_gate_job_launch` |
| `gate.merge_tree_clean` | `repository.merge_tree` | `head:word`, `base:word` | `clean`, `conflict`, `unavailable` | hard | repository reader | `pr_gate_job_launch` |
| `gate.controls_nonvacuous` | `gate.control_set` | `job:word` | `pass`, `fail`, `not_run`, `unavailable` | hard | control runner | `pr_gate_job_launch` |
| `gate.admission_passed` | `execution.admission` | `lane:word`, `resource:word` | `allowed`, `refused`, `unavailable` | hard | resource admission reader | `pr_gate_job_launch` |
| `gate.job_launch_accepted` | `gate.job_launch` | `job:word`, `head:word` | `launched`, `refused`, `unknown` | hard | job queue reader | `pr_gate_job_launch` |
| `gate.completion_watch_armed` | `gate.completion_watch` | `job:word` | `armed`, `absent`, `refused` | hard | completion monitor reader | `pr_gate_job_launch` |
| `gate.end_record_passed` | `gate.end_record` | `job:word` | `pass`, `fail`, `incomplete`, `unavailable` | hard | gate log reader | `pr_gate_job_launch` |
| `gate.pr_body_clear` | `pull_request.body_gate` | `head:word` | `clear`, `blocked`, `unavailable` | hard | pull request body scanner | `pr_gate_job_launch` |
| `gate.pr_body_matches_result` | `pull_request.body_claims` | `head:word` | `match`, `mismatch`, `unavailable` | hard | pull request body reader | `pr_gate_job_launch` |
| `gate.commit_attribution_verified` | `git.commit_attribution` | `branch:word`, `commit:word` | `match`, `mismatch`, `unavailable` | hard | commit metadata reader | `pr_gate_job_launch` |
| `gate.push_target_matches` | `git.push_target` | `branch:word`, `expected_head:word` | `match`, `mismatch`, `unavailable` | hard | remote branch reader | `pr_gate_job_launch` |
| `reply.thread_read_complete` | `message.thread` | `message:word` | `complete`, `partial`, `unreadable` | hard | mailbox thread reader | `reply_after_verification` |
| `reply.fact_verified` | `message.claim_check` | `claim:word`, `source:word` | `verified`, `refuted`, `unavailable` | hard | source reader | `reply_after_verification` |
| `reply.outbound_content_gate` | `message.outbound_gate` | `message:word` | `pass`, `blocked`, `unavailable` | hard | outbound content gate reader | `reply_after_verification` |
| `reply.delivery_confirmed` | `message.delivery` | `message:word` | `delivered`, `failed`, `pending` | advisory | message delivery reader | `reply_after_verification` |
| `reply.send_result_checked` | `message.send_result` | `message:word` | `accepted`, `refused`, `unknown` | advisory | communication result reader | `reply_after_verification` |
| `reply.correction_needed` | `message.claim_correction` | `message:word` | `correct`, `false`, `uncertain` | hard | evidence reconciliation reader | `reply_after_verification` |
| `reply.correction_delivered` | `message.correction_delivery` | `message:word`, `recipient:word` | `delivered`, `failed`, `pending` | advisory | message delivery reader | `reply_after_verification` |
| `reply.inbox_marked_after_delivery` | `inbox.reply_read_order` | `message:word` | `yes`, `no`, `unknown` | advisory | mailbox state reader | `reply_after_verification` |
