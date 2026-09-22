# Built-in operation access classification

This is the explicit operation table for `[gate].deny_writes_for`, defined by
[ADR-129 Amendment 3](../../../../docs/adr/ADR-129-fail-closed-gate-default.md).
`Read` permits retrieval/calculation and existing incidental audit, cache, and
maintenance effects. `Write` covers caller-requested domain mutations, lifecycle
and control changes, ingestion, external effects, generated persistent artifacts,
and explicit maintenance. Speech-act categories do not determine access.

`gtd.census` performs an aggregate SQL read on the bound notes backend and does
not acquire a writer, repair timestamps, or infer their units.

Only an explicit `Read` permits an enrolled, pattern-matched caller. Unknown,
unloaded, dynamically mounted, or third-party names without a classification are
denied. Internal subhandlers and aliases receive no exemption. Handler additions
must pass the production registry census; unknown-to-deny fallback does not count
as an explicit classification. Optional moodboard handlers are included here even
when that feature is not built. Workspace and formal declare no handlers.

The implementation is [operation.rs](../../src/operation.rs). Its classifier
version is part of every nonempty restriction's policy fingerprint.

| Exact name                   | Access | Surface    | Registration                                                                          |
| ---------------------------- | ------ | ---------- | ------------------------------------------------------------------------------------- |
| `agent.kill`                 | Write  | Verb       | [khive-pack-agent/src/pack.rs](../../../khive-pack-agent/src/pack.rs#L101)            |
| `agent.observe`              | Read   | Verb       | [khive-pack-agent/src/pack.rs](../../../khive-pack-agent/src/pack.rs#L60)             |
| `agent.resume`               | Write  | Verb       | [khive-pack-agent/src/pack.rs](../../../khive-pack-agent/src/pack.rs#L88)             |
| `agent.spawn`                | Write  | Verb       | [khive-pack-agent/src/pack.rs](../../../khive-pack-agent/src/pack.rs#L13)             |
| `agent.suspend`              | Write  | Verb       | [khive-pack-agent/src/pack.rs](../../../khive-pack-agent/src/pack.rs#L74)             |
| `blob.abort`                 | Write  | Verb       | [khive-pack-blob/src/pack.rs](../../../khive-pack-blob/src/pack.rs#L133)              |
| `blob.begin`                 | Write  | Verb       | [khive-pack-blob/src/pack.rs](../../../khive-pack-blob/src/pack.rs#L67)               |
| `blob.commit`                | Write  | Verb       | [khive-pack-blob/src/pack.rs](../../../khive-pack-blob/src/pack.rs#L118)              |
| `blob.get`                   | Read   | Verb       | [khive-pack-blob/src/pack.rs](../../../khive-pack-blob/src/pack.rs#L28)               |
| `blob.put`                   | Write  | Verb       | [khive-pack-blob/src/pack.rs](../../../khive-pack-blob/src/pack.rs#L13)               |
| `blob.put_part`              | Write  | Verb       | [khive-pack-blob/src/pack.rs](../../../khive-pack-blob/src/pack.rs#L89)               |
| `blob.stat`                  | Read   | Verb       | [khive-pack-blob/src/pack.rs](../../../khive-pack-blob/src/pack.rs#L52)               |
| `brain.activate`             | Write  | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L233)    |
| `brain.archive`              | Write  | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L259)    |
| `brain.auto_feedback`        | Write  | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L347)    |
| `brain.bind`                 | Write  | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L495)    |
| `brain.bindings`             | Read   | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L574)    |
| `brain.config`               | Read   | Subhandler | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L48)     |
| `brain.create_profile`       | Write  | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L610)    |
| `brain.deactivate`           | Write  | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L246)    |
| `brain.emit`                 | Write  | Subhandler | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L684)    |
| `brain.event_counts`         | Read   | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L77)     |
| `brain.events`               | Read   | Subhandler | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L61)     |
| `brain.feedback`             | Write  | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L285)    |
| `brain.mark_turn`            | Write  | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L473)    |
| `brain.profile`              | Read   | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L190)    |
| `brain.profiles`             | Read   | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L177)    |
| `brain.record_serve`         | Write  | Subhandler | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L425)    |
| `brain.register_adapter`     | Write  | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L646)    |
| `brain.reset`                | Write  | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L272)    |
| `brain.resolve`              | Read   | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L203)    |
| `brain.state`                | Read   | Subhandler | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L41)     |
| `brain.unbind`               | Write  | Verb       | [khive-pack-brain/src/handlers.rs](../../../khive-pack-brain/src/handlers.rs#L538)    |
| `code.ingest`                | Write  | Verb       | [khive-pack-code/src/vocab.rs](../../../khive-pack-code/src/vocab.rs#L11)             |
| `comm.cursor_commit`         | Write  | Subhandler | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L675)            |
| `comm.cursor_get`            | Read   | Subhandler | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L651)            |
| `comm.delivered`             | Read   | Verb       | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L104)            |
| `comm.health`                | Read   | Verb       | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L569)            |
| `comm.heartbeat`             | Write  | Subhandler | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L485)            |
| `comm.inbox`                 | Read   | Verb       | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L122)            |
| `comm.ingest`                | Write  | Subhandler | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L379)            |
| `comm.mark_read`             | Write  | Verb       | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L271)            |
| `comm.probe`                 | Read   | Verb       | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L609)            |
| `comm.read`                  | Write  | Verb       | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L249)            |
| `comm.reply`                 | Write  | Verb       | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L300)            |
| `comm.send`                  | Write  | Verb       | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L47)             |
| `comm.thread`                | Read   | Verb       | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L336)            |
| `comm.unread`                | Read   | Verb       | [khive-pack-comm/src/vocab.rs](../../../khive-pack-comm/src/vocab.rs#L293)            |
| `context`                    | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L1091) |
| `create`                     | Write  | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L71)   |
| `db_diagnostics`             | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L1436) |
| `delete`                     | Write  | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L594)  |
| `exec.events`                | Read   | Verb       | [khive-pack-exec/src/vocab.rs](../../../khive-pack-exec/src/vocab.rs#L136)            |
| `exec.identity`              | Read   | Verb       | [khive-pack-exec/src/vocab.rs](../../../khive-pack-exec/src/vocab.rs#L147)            |
| `exec.receipt`               | Read   | Verb       | [khive-pack-exec/src/vocab.rs](../../../khive-pack-exec/src/vocab.rs#L113)            |
| `exec.run`                   | Write  | Verb       | [khive-pack-exec/src/vocab.rs](../../../khive-pack-exec/src/vocab.rs#L95)             |
| `exec.runs`                  | Read   | Verb       | [khive-pack-exec/src/vocab.rs](../../../khive-pack-exec/src/vocab.rs#L123)            |
| `exec.tree`                  | Write  | Verb       | [khive-pack-exec/src/vocab.rs](../../../khive-pack-exec/src/vocab.rs#L56)             |
| `exec.tree_diff`             | Read   | Verb       | [khive-pack-exec/src/vocab.rs](../../../khive-pack-exec/src/vocab.rs#L84)             |
| `exec.tree_get`              | Read   | Verb       | [khive-pack-exec/src/vocab.rs](../../../khive-pack-exec/src/vocab.rs#L66)             |
| `exec.tree_put`              | Write  | Verb       | [khive-pack-exec/src/vocab.rs](../../../khive-pack-exec/src/vocab.rs#L73)             |
| `get`                        | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L203)  |
| `git.branch`                 | Write  | Verb       | [khive-pack-git/src/vocab.rs](../../../khive-pack-git/src/vocab.rs#L200)              |
| `git.checkout`               | Write  | Verb       | [khive-pack-git/src/local_vocab.rs](../../../khive-pack-git/src/local_vocab.rs#L30)   |
| `git.commit`                 | Write  | Verb       | [khive-pack-git/src/vocab.rs](../../../khive-pack-git/src/vocab.rs#L157)              |
| `git.diff`                   | Write  | Verb       | [khive-pack-git/src/local_vocab.rs](../../../khive-pack-git/src/local_vocab.rs#L37)   |
| `git.digest`                 | Write  | Verb       | [khive-pack-git/src/vocab.rs](../../../khive-pack-git/src/vocab.rs#L106)              |
| `git.gates`                  | Read   | Verb       | [khive-pack-git/src/local_vocab.rs](../../../khive-pack-git/src/local_vocab.rs#L57)   |
| `git.ingest_cursor`          | Read   | Verb       | [khive-pack-git/src/vocab.rs](../../../khive-pack-git/src/vocab.rs#L84)               |
| `git.init`                   | Write  | Verb       | [khive-pack-git/src/local_vocab.rs](../../../khive-pack-git/src/local_vocab.rs#L96)   |
| `git.log`                    | Read   | Verb       | [khive-pack-git/src/local_vocab.rs](../../../khive-pack-git/src/local_vocab.rs#L83)   |
| `git.pr_merge`               | Write  | Verb       | [khive-pack-git/src/remote_vocab.rs](../../../khive-pack-git/src/remote_vocab.rs#L78) |
| `git.pr_open`                | Write  | Verb       | [khive-pack-git/src/remote_vocab.rs](../../../khive-pack-git/src/remote_vocab.rs#L47) |
| `git.pr_review`              | Write  | Verb       | [khive-pack-git/src/remote_vocab.rs](../../../khive-pack-git/src/remote_vocab.rs#L63) |
| `git.push`                   | Write  | Verb       | [khive-pack-git/src/remote_vocab.rs](../../../khive-pack-git/src/remote_vocab.rs#L33) |
| `git.receipts`               | Read   | Verb       | [khive-pack-git/src/local_vocab.rs](../../../khive-pack-git/src/local_vocab.rs#L44)   |
| `git.reconcile`              | Write  | Verb       | [khive-pack-git/src/local_vocab.rs](../../../khive-pack-git/src/local_vocab.rs#L64)   |
| `git.status`                 | Read   | Verb       | [khive-pack-git/src/local_vocab.rs](../../../khive-pack-git/src/local_vocab.rs#L72)   |
| `gtd.assign`                 | Write  | Verb       | [khive-pack-gtd/src/vocab.rs](../../../khive-pack-gtd/src/vocab.rs#L98)               |
| `gtd.census`                 | Read   | Verb       | [khive-pack-gtd/src/vocab.rs](../../../khive-pack-gtd/src/vocab.rs#L98)               |
| `gtd.complete`               | Write  | Verb       | [khive-pack-gtd/src/vocab.rs](../../../khive-pack-gtd/src/vocab.rs#L225)              |
| `gtd.next`                   | Read   | Verb       | [khive-pack-gtd/src/vocab.rs](../../../khive-pack-gtd/src/vocab.rs#L191)              |
| `gtd.tasks`                  | Read   | Verb       | [khive-pack-gtd/src/vocab.rs](../../../khive-pack-gtd/src/vocab.rs#L271)              |
| `gtd.transition`             | Write  | Verb       | [khive-pack-gtd/src/vocab.rs](../../../khive-pack-gtd/src/vocab.rs#L353)              |
| `knowledge.adjudicate`       | Write  | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L578)  |
| `knowledge.challenge`        | Write  | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L542)  |
| `knowledge.cite`             | Write  | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L651)  |
| `knowledge.compose`          | Read   | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L421)  |
| `knowledge.delete_atoms`     | Write  | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L158)  |
| `knowledge.edit`             | Write  | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L488)  |
| `knowledge.eval_retrieval`   | Write  | Subhandler | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L187)  |
| `knowledge.feedback`         | Write  | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L714)  |
| `knowledge.fold`             | Read   | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L236)  |
| `knowledge.get`              | Read   | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L63)   |
| `knowledge.import`           | Write  | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L512)  |
| `knowledge.index`            | Write  | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L200)  |
| `knowledge.learn`            | Write  | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L615)  |
| `knowledge.list`             | Read   | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L85)   |
| `knowledge.search`           | Read   | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L286)  |
| `knowledge.stats`            | Read   | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L180)  |
| `knowledge.suggest`          | Read   | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L392)  |
| `knowledge.topic`            | Read   | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L684)  |
| `knowledge.upsert_atoms`     | Write  | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L28)   |
| `knowledge.upsert_domains`   | Write  | Verb       | [khive-pack-knowledge/src/vocab.rs](../../../khive-pack-knowledge/src/vocab.rs#L50)   |
| `link`                       | Write  | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L809)  |
| `list`                       | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L244)  |
| `memory.feedback`            | Write  | Verb       | [khive-pack-memory/src/pack.rs](../../../khive-pack-memory/src/pack.rs#L173)          |
| `memory.prune`               | Write  | Verb       | [khive-pack-memory/src/pack.rs](../../../khive-pack-memory/src/pack.rs#L390)          |
| `memory.recall`              | Read   | Verb       | [khive-pack-memory/src/pack.rs](../../../khive-pack-memory/src/pack.rs#L196)          |
| `memory.recall_candidates`   | Read   | Subhandler | [khive-pack-memory/src/pack.rs](../../../khive-pack-memory/src/pack.rs#L360)          |
| `memory.recall_embed`        | Read   | Subhandler | [khive-pack-memory/src/pack.rs](../../../khive-pack-memory/src/pack.rs#L347)          |
| `memory.recall_fuse`         | Read   | Subhandler | [khive-pack-memory/src/pack.rs](../../../khive-pack-memory/src/pack.rs#L367)          |
| `memory.recall_rerank`       | Read   | Subhandler | [khive-pack-memory/src/pack.rs](../../../khive-pack-memory/src/pack.rs#L375)          |
| `memory.recall_score`        | Read   | Subhandler | [khive-pack-memory/src/pack.rs](../../../khive-pack-memory/src/pack.rs#L382)          |
| `memory.remember`            | Write  | Verb       | [khive-pack-memory/src/pack.rs](../../../khive-pack-memory/src/pack.rs#L94)           |
| `memory.vacuum`              | Write  | Verb       | [khive-pack-memory/src/pack.rs](../../../khive-pack-memory/src/pack.rs#L429)          |
| `merge`                      | Write  | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L648)  |
| `moodboard.ingest`           | Write  | Verb       | [khive-pack-moodboard/src/vocab.rs](../../../khive-pack-moodboard/src/vocab.rs#L43)   |
| `moodboard.judge`            | Write  | Verb       | [khive-pack-moodboard/src/vocab.rs](../../../khive-pack-moodboard/src/vocab.rs#L173)  |
| `moodboard.model`            | Read   | Verb       | [khive-pack-moodboard/src/vocab.rs](../../../khive-pack-moodboard/src/vocab.rs#L34)   |
| `moodboard.preference`       | Read   | Verb       | [khive-pack-moodboard/src/vocab.rs](../../../khive-pack-moodboard/src/vocab.rs#L262)  |
| `moodboard.search`           | Read   | Verb       | [khive-pack-moodboard/src/vocab.rs](../../../khive-pack-moodboard/src/vocab.rs#L83)   |
| `moodboard.serve`            | Write  | Verb       | [khive-pack-moodboard/src/vocab.rs](../../../khive-pack-moodboard/src/vocab.rs#L107)  |
| `moodboard.train_preference` | Write  | Verb       | [khive-pack-moodboard/src/vocab.rs](../../../khive-pack-moodboard/src/vocab.rs#L224)  |
| `neighbors`                  | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L944)  |
| `propose`                    | Write  | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L1207) |
| `query`                      | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L1174) |
| `resolve`                    | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L1331) |
| `restore`                    | Write  | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L625)  |
| `review`                     | Write  | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L1272) |
| `scan`                       | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L1401) |
| `schedule.agenda`            | Read   | Verb       | [khive-pack-schedule/src/vocab.rs](../../../khive-pack-schedule/src/vocab.rs#L106)    |
| `schedule.cancel`            | Write  | Verb       | [khive-pack-schedule/src/vocab.rs](../../../khive-pack-schedule/src/vocab.rs#L135)    |
| `schedule.remind`            | Write  | Verb       | [khive-pack-schedule/src/vocab.rs](../../../khive-pack-schedule/src/vocab.rs#L25)     |
| `schedule.schedule`          | Write  | Verb       | [khive-pack-schedule/src/vocab.rs](../../../khive-pack-schedule/src/vocab.rs#L54)     |
| `search`                     | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L716)  |
| `session.export`             | Read   | Verb       | [khive-pack-session/src/vocab.rs](../../../khive-pack-session/src/vocab.rs#L154)      |
| `session.list`               | Read   | Verb       | [khive-pack-session/src/vocab.rs](../../../khive-pack-session/src/vocab.rs#L98)       |
| `session.resume`             | Read   | Verb       | [khive-pack-session/src/vocab.rs](../../../khive-pack-session/src/vocab.rs#L141)      |
| `session.store`              | Write  | Verb       | [khive-pack-session/src/vocab.rs](../../../khive-pack-session/src/vocab.rs#L55)       |
| `stats`                      | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L475)  |
| `stream.append`              | Write  | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L18)   |
| `stream.batch`               | Write  | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L57)   |
| `stream.read`                | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L35)   |
| `stream.stat`                | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L47)   |
| `telemetry.channels`         | Read   | Verb       | [khive-pack-telemetry/src/pack.rs](../../../khive-pack-telemetry/src/pack.rs#L9)      |
| `telemetry.counts`           | Read   | Verb       | [khive-pack-telemetry/src/pack.rs](../../../khive-pack-telemetry/src/pack.rs#L113)    |
| `telemetry.emit`             | Write  | Verb       | [khive-pack-telemetry/src/pack.rs](../../../khive-pack-telemetry/src/pack.rs#L18)     |
| `telemetry.read`             | Read   | Verb       | [khive-pack-telemetry/src/pack.rs](../../../khive-pack-telemetry/src/pack.rs#L60)     |
| `tool.check`                 | Read   | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L186)            |
| `tool.deny`                  | Write  | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L219)            |
| `tool.describe`              | Read   | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L167)            |
| `tool.grant`                 | Write  | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L207)            |
| `tool.ingest`                | Write  | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L141)            |
| `tool.list`                  | Read   | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L174)            |
| `tool.policies`              | Read   | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L267)            |
| `tool.policy`                | Write  | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L254)            |
| `tool.register`              | Write  | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L123)            |
| `tool.request`               | Write  | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L193)            |
| `tool.requests`              | Read   | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L241)            |
| `tool.revoke`                | Write  | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L230)            |
| `tool.suggest`               | Read   | Verb       | [khive-pack-tool/src/vocab.rs](../../../khive-pack-tool/src/vocab.rs#L154)            |
| `traverse`                   | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L1017) |
| `update`                     | Write  | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L495)  |
| `verbs`                      | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L1473) |
| `web.ingest`                 | Write  | Verb       | [khive-pack-web/src/vocab.rs](../../../khive-pack-web/src/vocab.rs#L6)                |
| `whoami`                     | Read   | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L1387) |
| `withdraw`                   | Write  | Verb       | [khive-pack-kg/src/handler_defs.rs](../../../khive-pack-kg/src/handler_defs.rs#L1304) |

## Runtime pseudo-verbs

| Exact name          | Access | Authority                                                                                                |
| ------------------- | ------ | -------------------------------------------------------------------------------------------------------- |
| `authorize`         | Write  | Mints a primary read/write token; both `authorize` and `authorize_with_visibility` check this name.      |
| `authorize.visible` | Read   | Checks one additional read namespace only after the primary broad-token check; alone it grants no token. |

See [runtime token minting](../../../khive-runtime/src/runtime.rs#L1086).

## Effects that names alone do not reveal

- `comm.read` and `comm.mark_read` change read flags. Both are Write.
  `brain.emit` is the deprecated Write alias of `brain.feedback`.
- `git.diff` and `git.checkout` publish artifacts/receipts; `git.reconcile` changes
  receipt state. `exec.tree` persists a manifest. They are Write despite possible
  read-oriented names. `exec.tree_diff` only returns a comparison and is Read.
- `knowledge.fold` computes a result from supplied candidates and is Read;
  `knowledge.eval_retrieval` persists evaluation runs and is Write.
- `session.resume` and `session.export` return stored data; they do not resume a
  process or write an export file. `agent.resume` controls a process and is Write.
- `memory.vacuum` explicitly requests maintenance and is Write. `db_diagnostics`
  is Read, including its existing PASSIVE checkpoint I/O; `comm.cursor_get` may
  lazily initialize its cursor schema. Search/recall may persist normal telemetry.
- `memory.recall` is Read, but its nested `brain.record_serve` dispatch keeps the
  same actor and is Write. For a restricted actor the ledger call is denied and
  warned; recall results still return, and its direct RecallExecuted telemetry
  may persist. There is no internal privilege bypass. See
  [recall tracking](../../../khive-pack-memory/src/handlers/recall.rs#L1143).
- Existing `help=true` schema introspection returns before the gate and handler;
  it may describe a Write verb but cannot execute it.

Dry-run, no-op, or abstention arguments do not weaken a Write classification.
This contract does not provide zero persistent-write read-only execution, live
revocation of held tokens, or isolation from a same-UID caller changing its
configuration/actor label.
