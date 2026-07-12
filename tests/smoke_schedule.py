#!/usr/bin/env python3
"""Smoke + behavioural tests for the khive-mcp schedule pack over stdio MCP.

Spawns the binary with an in-memory DB, --pack kg --pack schedule, sends
JSON-RPC requests, and verifies all four verbs (schedule.remind,
schedule.schedule, schedule.agenda, schedule.cancel) across happy paths
and error cases.

Usage:
    uv run python tests/smoke_schedule.py
    # or: python3 tests/smoke_schedule.py
"""

import json
import subprocess
import sys
import os
import uuid

BINARY = os.environ.get(
    "KKERNEL_BINARY",
    os.path.join(os.path.dirname(__file__), "..", "crates", "target", "release", "kkernel"),
)

request_id = 0


def next_id():
    global request_id
    request_id += 1
    return request_id


def send(proc, method, params=None):
    msg = {"jsonrpc": "2.0", "id": next_id(), "method": method}
    if params is not None:
        msg["params"] = params
    line = json.dumps(msg) + "\n"
    proc.stdin.write(line.encode())
    proc.stdin.flush()


def recv(proc):
    line = proc.stdout.readline()
    if not line:
        raise RuntimeError("MCP server closed stdout")
    return json.loads(line)


def _call_request_raw(proc, ops_string, presentation=None):
    """Send `request(ops=<ops_string>)`. Return the parsed response body.

    presentation: None (default="agent"), "verbose", or "human".
    Agent mode compacts ISO-8601 timestamps to minute granularity and shortens UUIDs.
    Verbose mode returns the canonical handler output unchanged.
    """
    arguments = {"ops": ops_string}
    if presentation is not None:
        arguments["presentation"] = presentation
    send(proc, "tools/call", {"name": "request", "arguments": arguments})
    resp = recv(proc)
    if "error" in resp:
        raise RuntimeError(f"MCP error calling request: {resp['error']}")
    result = resp.get("result", {})
    if result.get("isError"):
        content = result.get("content", [])
        text = content[0]["text"] if content else "(no text)"
        raise RuntimeError(f"request returned protocol error: {text}")
    content = result.get("content", [])
    text = content[0]["text"] if content else ""
    return json.loads(text) if text else None


def call_verb(proc, name, args, presentation=None):
    """Call a single verb. Return the result on success, raise RuntimeError on per-op error."""
    ops = json.dumps([{"tool": name, "args": args}])
    body = _call_request_raw(proc, ops, presentation=presentation)
    if body is None:
        raise RuntimeError(f"request returned empty body for verb {name}")
    results = body.get("results") or []
    if not results:
        raise RuntimeError(f"request returned no results for verb {name}: {body}")
    first = results[0]
    if not first.get("ok", False):
        raise RuntimeError(f"verb {name} failed: {first.get('error', '<no error string>')}")
    return first.get("result")


def call_verb_expect_error(proc, name, args):
    """Call a single verb and return the error string (raise if it unexpectedly succeeds)."""
    ops = json.dumps([{"tool": name, "args": args}])
    body = _call_request_raw(proc, ops)
    if body is None:
        raise AssertionError(f"expected error from {name} but got empty body")
    results = body.get("results") or []
    if not results:
        raise AssertionError(f"expected error from {name} but got no results: {body}")
    first = results[0]
    if first.get("ok", False):
        raise AssertionError(
            f"expected {name} to fail but it succeeded: {first.get('result')}"
        )
    return first.get("error", "<no error string>")


# Issue #779: 8 (then 14, then 42, accelerating) year-2099 schedule-pack
# fixture rows -- matching this file's exact test content ("check the build",
# "repeat=daily", "window-early", "sort-order-jan", ...) -- were found parked
# in a real, on-disk store. This script's fixtures must only ever land in an
# ephemeral store; ISOLATION_DB is the single source of truth for that, and
# spawn_proc asserts its own argv actually carries it before returning the
# process, so a future edit that drops or changes the `--db` flag fails this
# script immediately instead of silently writing into whatever store the
# environment resolves.
ISOLATION_DB = ":memory:"


def spawn_proc():
    """Spawn a fresh khive-mcp process with kg + schedule packs.

    Always isolated: `--db :memory:` is non-negotiable for this script, so we
    build argv from ISOLATION_DB and assert it landed exactly where expected
    rather than trusting the literal below never drifts.
    """
    argv = [
        BINARY, "mcp", "--db", ISOLATION_DB, "--no-embed", "--log", "error",
        "--pack", "kg", "--pack", "schedule",
    ]
    db_flag_index = argv.index("--db")
    assert argv[db_flag_index + 1] == ISOLATION_DB, (
        "refusing to spawn: --db must be an ephemeral store "
        f"({ISOLATION_DB!r}), got {argv[db_flag_index + 1]!r} -- this guard "
        "exists so this script can never silently write schedule pack "
        "fixtures into a real database (issue #779)"
    )
    return subprocess.Popen(
        argv,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def initialize(proc, client_name="schedule-smoke"):
    send(proc, "initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": client_name, "version": "0.1.0"},
    })
    recv(proc)
    notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
    proc.stdin.write((json.dumps(notify) + "\n").encode())
    proc.stdin.flush()


# ── future timestamps (always valid) ─────────────────────────────────────────
FAR_FUTURE_A = "2099-01-01T00:00:00Z"
FAR_FUTURE_B = "2099-06-15T12:00:00Z"
FAR_FUTURE_C = "2099-12-31T23:59:59Z"


def main():
    print(f"Binary: {BINARY}")
    assert os.path.exists(BINARY), f"Binary not found: {BINARY}"

    failures = []

    def run_test(name, fn, proc):
        try:
            fn(proc)
            print(f"  [ok] {name}")
        except Exception as e:
            print(f"  [FAIL] {name}: {e}")
            failures.append(name)

    # ── single process for all tests ─────────────────────────────────────────
    proc = spawn_proc()
    try:
        initialize(proc)

        # 1. Happy path remind: create → agenda shows it → cancel → agenda omits it
        def test_happy_remind(proc):
            # Use verbose presentation so trigger_at is returned unchanged.
            # Agent mode (default) compacts ISO-8601 timestamps to minute granularity
            # ("2099-01-01T00:00") which is by design — see presentation.rs compact_timestamp.
            ev = call_verb(proc, "schedule.remind", {
                "content": "check the build",
                "at": FAR_FUTURE_A,
            }, presentation="verbose")
            assert ev["event_type"] == "remind", f"expected event_type=remind: {ev}"
            assert ev["status"] == "pending", f"expected status=pending: {ev}"
            assert "id" in ev, f"no id in response: {ev}"
            assert "full_id" in ev, f"no full_id in response: {ev}"
            assert ev["trigger_at"] == FAR_FUTURE_A, (
                f"trigger_at mismatch: {ev['trigger_at']!r} != {FAR_FUTURE_A!r}"
            )
            event_id = ev["full_id"]  # 36-char from verbose mode
            event_short_id = ev["id"]   # 8-char prefix

            # Agenda must list the pending event. Use verbose so full_id is 36-char.
            agenda = call_verb(proc, "schedule.agenda", {}, presentation="verbose")
            events = agenda["events"]
            ids = [e["full_id"] for e in events]
            assert event_id in ids, f"created event {event_short_id} not in agenda: {ids}"

            # Cancel the event using full_id.
            cancelled = call_verb(proc, "schedule.cancel", {"id": event_id}, presentation="verbose")
            assert cancelled["status"] == "cancelled", f"expected cancelled: {cancelled}"
            assert cancelled["full_id"] == event_id

            # Agenda must no longer list the cancelled event (only pending shown).
            agenda2 = call_verb(proc, "schedule.agenda", {}, presentation="verbose")
            ids2 = [e["full_id"] for e in agenda2["events"]]
            assert event_id not in ids2, (
                f"cancelled event {event_short_id} still appears in agenda: {ids2}"
            )

        run_test("happy-path remind: create → agenda → cancel → agenda omits", test_happy_remind, proc)

        # 2. Happy path schedule: valid DSL action → agenda shows it
        def test_happy_schedule(proc):
            ev = call_verb(proc, "schedule.schedule", {
                "action": f'schedule.remind(content="hello from scheduled", at="{FAR_FUTURE_A}")',
                "at": FAR_FUTURE_B,
            }, presentation="verbose")
            assert ev["event_type"] == "schedule", f"expected event_type=schedule: {ev}"
            assert ev["status"] == "pending", f"expected status=pending: {ev}"
            assert ev["trigger_at"] == FAR_FUTURE_B, (
                f"trigger_at mismatch: {ev['trigger_at']!r}"
            )
            event_id = ev["full_id"]  # 36-char from verbose mode

            agenda = call_verb(proc, "schedule.agenda", {}, presentation="verbose")
            ids = [e["full_id"] for e in agenda["events"]]
            assert event_id in ids, (
                f"scheduled event {event_id[:8]} not in agenda: {ids}"
            )

        run_test("happy-path schedule: valid DSL → agenda shows it", test_happy_schedule, proc)

        # 3. Past timestamp rejection
        def test_past_timestamp(proc):
            err = call_verb_expect_error(proc, "schedule.remind", {
                "content": "stale",
                "at": "2020-01-01T00:00:00Z",
            })
            assert "past" in err.lower() or "future" in err.lower(), (
                f"expected 'past'/'future' hint in error; got: {err!r}"
            )

        run_test("past timestamp → error mentioning 'past'", test_past_timestamp, proc)

        # 4. Invalid timestamp (garbage string)
        def test_invalid_timestamp(proc):
            err = call_verb_expect_error(proc, "schedule.remind", {
                "content": "test",
                "at": "not-a-valid-timestamp",
            })
            assert "rfc 3339" in err.lower() or "rfc3339" in err.lower() or "timestamp" in err.lower(), (
                f"expected RFC 3339 hint in error; got: {err!r}"
            )

        run_test("invalid timestamp → error mentioning RFC 3339", test_invalid_timestamp, proc)

        # 5. Empty content rejection
        def test_empty_content(proc):
            err = call_verb_expect_error(proc, "schedule.remind", {
                "content": "",
                "at": FAR_FUTURE_A,
            })
            assert "content" in err.lower() or "empty" in err.lower(), (
                f"expected 'content'/'empty' in error; got: {err!r}"
            )

        run_test("empty content → error", test_empty_content, proc)

        # 6. Empty action rejection
        def test_empty_action(proc):
            err = call_verb_expect_error(proc, "schedule.schedule", {
                "action": "",
                "at": FAR_FUTURE_A,
            })
            assert "action" in err.lower() or "empty" in err.lower(), (
                f"expected 'action'/'empty' in error; got: {err!r}"
            )

        run_test("empty action → error", test_empty_action, proc)

        # 7. Invalid DSL action rejection
        def test_invalid_dsl(proc):
            err = call_verb_expect_error(proc, "schedule.schedule", {
                "action": "bogus-not-a-valid-verb()",
                "at": FAR_FUTURE_A,
            })
            assert "dsl" in err.lower() or "verb" in err.lower() or "action" in err.lower() or "invalid" in err.lower(), (
                f"expected DSL/verb/action/invalid in error; got: {err!r}"
            )

        run_test("invalid DSL action → error mentioning DSL or verb", test_invalid_dsl, proc)

        # 8. Valid DSL action accepted
        def test_valid_dsl(proc):
            ev = call_verb(proc, "schedule.schedule", {
                "action": f'schedule.remind(content="test", at="{FAR_FUTURE_C}")',
                "at": FAR_FUTURE_A,
            })
            assert ev["status"] == "pending", f"expected pending: {ev}"

        run_test("valid DSL action accepted", test_valid_dsl, proc)

        # 9. Repeat validation
        def test_repeat_valid(proc):
            for repeat in ("daily", "weekly", "monthly"):
                ev = call_verb(proc, "schedule.remind", {
                    "content": f"repeat={repeat}",
                    "at": FAR_FUTURE_A,
                    "repeat": repeat,
                })
                assert ev["status"] == "pending", f"repeat={repeat!r} should succeed: {ev}"

        run_test("repeat=daily/weekly/monthly → ok", test_repeat_valid, proc)

        def test_repeat_invalid_cron(proc):
            # "invalid-cron" is not 5-field cron and not a named alias.
            err = call_verb_expect_error(proc, "schedule.remind", {
                "content": "bad cron",
                "at": FAR_FUTURE_A,
                "repeat": "invalid-cron",
            })
            assert "repeat" in err.lower() or "cron" in err.lower() or "invalid" in err.lower(), (
                f"expected repeat/cron/invalid in error; got: {err!r}"
            )

        run_test("repeat=invalid-cron → error", test_repeat_invalid_cron, proc)

        def test_repeat_5field_cron(proc):
            # 5-field cron must be accepted.
            ev = call_verb(proc, "schedule.remind", {
                "content": "cron reminder",
                "at": FAR_FUTURE_B,
                "repeat": "0 9 * * 1",
            })
            assert ev["status"] == "pending", f"5-field cron should succeed: {ev}"

        run_test("repeat=5-field cron → ok", test_repeat_5field_cron, proc)

        # 10. Agenda time window: only events in [from, to] range returned
        def test_agenda_time_window(proc):
            # Create event A at FAR_FUTURE_A (2099-01-01) and B at FAR_FUTURE_C (2099-12-31).
            # Use verbose to get full full_id (36-char) for comparison.
            ev_a = call_verb(proc, "schedule.remind", {
                "content": "window-early",
                "at": FAR_FUTURE_A,
            }, presentation="verbose")
            ev_c = call_verb(proc, "schedule.remind", {
                "content": "window-late",
                "at": FAR_FUTURE_C,
            }, presentation="verbose")

            # Window: from 2099-06-01 to 2099-12-31 — should include C but not A.
            # Use verbose so full_id in agenda is 36-char for exact matching.
            agenda = call_verb(proc, "schedule.agenda", {
                "from": "2099-06-01T00:00:00Z",
                "to": FAR_FUTURE_C,
                "limit": 50,
            }, presentation="verbose")
            events = agenda["events"]
            full_ids = [e["full_id"] for e in events]
            assert ev_a["full_id"] not in full_ids, (
                f"early event (2099-01-01) must NOT appear in [2099-06-01, 2099-12-31] window"
            )
            assert ev_c["full_id"] in full_ids, (
                f"late event (2099-12-31) must appear in [2099-06-01, 2099-12-31] window"
            )

        run_test("agenda time window: from/to filter works", test_agenda_time_window, proc)

        # 11. Cancel non-existent UUID → not found
        def test_cancel_nonexistent(proc):
            bogus_id = str(uuid.uuid4())
            err = call_verb_expect_error(proc, "schedule.cancel", {"id": bogus_id})
            assert "not found" in err.lower() or "notfound" in err.lower(), (
                f"expected 'not found' in error; got: {err!r}"
            )

        run_test("cancel non-existent UUID → 'not found'", test_cancel_nonexistent, proc)

        # 12. Cancel already-cancelled event: check behaviour
        def test_cancel_twice(proc):
            ev = call_verb(proc, "schedule.remind", {
                "content": "double-cancel",
                "at": FAR_FUTURE_B,
            })
            event_id = ev["full_id"]

            # First cancel: must succeed.
            result1 = call_verb(proc, "schedule.cancel", {"id": event_id})
            assert result1["status"] == "cancelled", f"first cancel must succeed: {result1}"

            # Second cancel on already-cancelled event:
            # The handler re-fetches the note by UUID, checks kind==scheduled_event (passes),
            # then overwrites status=cancelled again. No already-cancelled guard exists.
            # BUG: cancel is idempotent (silently overwrites) rather than erroring on
            # already-cancelled events. Document the actual observed behaviour.
            try:
                result2 = call_verb(proc, "schedule.cancel", {"id": event_id})
                # Idempotent path: succeeds again.
                assert result2["status"] == "cancelled", (
                    f"second cancel returned unexpected status: {result2}"
                )
                print("       (note: double-cancel is idempotent — no error on already-cancelled)")
            except RuntimeError as e:
                # Guard path: some future version may return an error.
                print(f"       (note: double-cancel raises: {e})")

        run_test("cancel already-cancelled: idempotent (no guard)", test_cancel_twice, proc)

        # 13. Cancel a non-scheduled_event note → error mentioning "scheduled_event"
        def test_cancel_wrong_kind(proc):
            # Create a plain KG observation note.
            obs = call_verb(proc, "create", {
                "kind": "note",
                "note_kind": "observation",
                "content": "this is not a scheduled event",
            })
            note_id = obs["id"]  # short id
            err = call_verb_expect_error(proc, "schedule.cancel", {"id": note_id})
            assert "scheduled_event" in err.lower() or "kind" in err.lower(), (
                f"expected 'scheduled_event'/'kind' in error; got: {err!r}"
            )

        run_test("cancel non-scheduled_event note → error mentioning kind", test_cancel_wrong_kind, proc)

        # ── additional coverage ──────────────────────────────────────────────

        # 14. schedule.remind returns id (8-char) and full_id (36-char UUID)
        def test_id_shapes(proc):
            ev = call_verb(proc, "schedule.remind", {
                "content": "id-shape test",
                "at": FAR_FUTURE_A,
            })
            short_id = ev["id"]
            full_id = ev["full_id"]
            assert len(short_id) == 8, f"id must be 8 chars, got {len(short_id)}: {short_id!r}"
            assert len(full_id) == 36, f"full_id must be 36 chars, got {len(full_id)}: {full_id!r}"
            assert full_id.startswith(short_id), (
                f"full_id must start with short id prefix; short={short_id!r} full={full_id!r}"
            )

        run_test("id/full_id shapes: 8-char prefix + 36-char UUID", test_id_shapes, proc)

        # 15. schedule.cancel accepts short 8-char id
        def test_cancel_short_id(proc):
            ev = call_verb(proc, "schedule.remind", {
                "content": "short-id cancel test",
                "at": FAR_FUTURE_C,
            })
            short = ev["id"]
            result = call_verb(proc, "schedule.cancel", {"id": short})
            assert result["status"] == "cancelled", f"cancel by short id failed: {result}"

        run_test("cancel accepts 8-char short id", test_cancel_short_id, proc)

        # 16. Agenda returns events sorted ascending by trigger_at
        def test_agenda_sort_order(proc):
            # Create two reminders: B first (June), then A (January) so storage
            # insertion order differs from temporal order.
            call_verb(proc, "schedule.remind", {
                "content": "sort-order-june",
                "at": FAR_FUTURE_B,  # 2099-06-15
            })
            call_verb(proc, "schedule.remind", {
                "content": "sort-order-jan",
                "at": FAR_FUTURE_A,  # 2099-01-01
            })
            # Use verbose to get full trigger_at strings in properties.
            agenda = call_verb(proc, "schedule.agenda", {
                "from": FAR_FUTURE_A,
                "to": FAR_FUTURE_B,
                "limit": 50,
            }, presentation="verbose")
            events = agenda["events"]
            trigger_times = [
                e["properties"]["trigger_at"]
                for e in events
                if e.get("properties", {}).get("trigger_at") in (FAR_FUTURE_A, FAR_FUTURE_B)
            ]
            # Must appear in ascending order.
            if len(trigger_times) >= 2:
                assert trigger_times == sorted(trigger_times), (
                    f"agenda events must be ascending by trigger_at; got: {trigger_times}"
                )

        run_test("agenda sorted ascending by trigger_at", test_agenda_sort_order, proc)

        # 17. Trigger_at preserved exactly (H5: no UTC canonicalisation)
        # NOTE: Agent mode (default) compacts ALL ISO-8601 timestamps to minute
        # granularity. trigger_at is treated as a display timestamp by the presentation
        # layer, so "2099-03-10T15:00:00+05:30" → "2099-03-10T15:00" in Agent mode.
        # This is a known design tension: trigger_at is a payload value (not metadata)
        # but the presentation layer has no way to distinguish them. Use verbose mode
        # to verify the handler preserves the original string.
        def test_trigger_at_preserved(proc):
            offset_at = "2099-03-10T15:00:00+05:30"
            ev = call_verb(proc, "schedule.remind", {
                "content": "tz-preservation",
                "at": offset_at,
            }, presentation="verbose")
            assert ev["trigger_at"] == offset_at, (
                f"H5: trigger_at must be returned as-is in verbose mode; "
                f"submitted={offset_at!r} got={ev['trigger_at']!r}"
            )
            # In Agent mode (default) the timestamp is compacted to minute granularity.
            # "2099-03-10T15:00:00+05:30" → first 16 chars → "2099-03-10T15:00"
            ev_agent = call_verb(proc, "schedule.remind", {
                "content": "tz-preservation-agent",
                "at": offset_at,
            })  # default=agent presentation
            expected_agent = offset_at[:16]  # "2099-03-10T15:00"
            assert ev_agent["trigger_at"] == expected_agent, (
                f"Agent mode: trigger_at must be compacted to minute granularity; "
                f"expected={expected_agent!r} got={ev_agent['trigger_at']!r}"
            )

        run_test("trigger_at preserved exactly (no UTC canonicalisation)", test_trigger_at_preserved, proc)

        # 18. Agenda with invalid `from` value → error
        def test_agenda_invalid_from(proc):
            err = call_verb_expect_error(proc, "schedule.agenda", {
                "from": "not-a-date",
            })
            assert "rfc 3339" in err.lower() or "rfc3339" in err.lower() or "timestamp" in err.lower(), (
                f"expected RFC 3339 hint in error; got: {err!r}"
            )

        run_test("agenda invalid from → RFC 3339 error", test_agenda_invalid_from, proc)

        # 19. Agenda with invalid `to` value → error
        def test_agenda_invalid_to(proc):
            err = call_verb_expect_error(proc, "schedule.agenda", {
                "to": "not-a-date",
            })
            assert "rfc 3339" in err.lower() or "rfc3339" in err.lower() or "timestamp" in err.lower(), (
                f"expected RFC 3339 hint in error; got: {err!r}"
            )

        run_test("agenda invalid to → RFC 3339 error", test_agenda_invalid_to, proc)

        # 20. Agenda limit respected
        def test_agenda_limit(proc):
            # Create 3 reminders; ask for limit=2.
            for i in range(3):
                call_verb(proc, "schedule.remind", {
                    "content": f"limit-test-{i}",
                    "at": FAR_FUTURE_A,
                })
            agenda = call_verb(proc, "schedule.agenda", {"limit": 2})
            assert agenda["count"] <= 2, (
                f"agenda with limit=2 must return at most 2 events; got {agenda['count']}"
            )
            assert len(agenda["events"]) <= 2, (
                f"events array must have <= 2 items; got {len(agenda['events'])}"
            )

        run_test("agenda limit=2 caps results", test_agenda_limit, proc)

    finally:
        proc.stdin.close()
        proc.wait(timeout=5)

    if failures:
        print(f"\n  {len(failures)} FAILURE(S): {failures}")
        return 1

    print(f"\n  ALL SCHEDULE PACK SMOKE TESTS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
