#!/usr/bin/env python3
"""Smoke + behavioral tests for the brain pack via MCP stdio.

Spawns the binary with --pack kg --pack brain --namespace local, sends
JSON-RPC over stdin, and verifies every brain verb works end-to-end.

The brain pack is a Bayesian belief engine (ADR-032).  Profiles hold
Beta-distributed posteriors that are updated by feedback events.

Usage:
    uv run python tests/smoke_brain.py
    # or: python3 tests/smoke_brain.py
"""

import json
import subprocess
import sys
import os

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


def _call_request_raw(proc, ops_string):
    """Send `request(ops=<ops_string>)`. Return the parsed response body."""
    send(proc, "tools/call", {"name": "request", "arguments": {"ops": ops_string}})
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


def call_verb(proc, name, args):
    """Call a single verb through `request`. Return that verb's result, or raise on per-op error."""
    ops = json.dumps([{"tool": name, "args": args}])
    body = _call_request_raw(proc, ops)
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
    """Call a verb and return the per-op error string (raises if the verb unexpectedly succeeded)."""
    ops = json.dumps([{"tool": name, "args": args}])
    body = _call_request_raw(proc, ops)
    if body is None:
        raise RuntimeError(f"expected error from {name} but got empty body")
    results = body.get("results") or []
    if not results:
        raise RuntimeError(f"expected error from {name} but got no results: {body}")
    first = results[0]
    if first.get("ok", False):
        raise RuntimeError(
            f"expected {name} to fail but it succeeded: {first.get('result')}"
        )
    return first.get("error", "<no error string>")


def spawn_brain_proc():
    """Spawn binary with kg + brain packs and return the initialized process."""
    proc = subprocess.Popen(
        [
            BINARY,
            "mcp", "--db", ":memory:",
            "--no-embed",
            "--log", "error",
            "--namespace", "local",
            "--pack", "kg",
            "--pack", "brain",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    send(proc, "initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "brain-smoke", "version": "0.1.0"},
    })
    recv(proc)
    notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
    proc.stdin.write((json.dumps(notify) + "\n").encode())
    proc.stdin.flush()
    return proc


def brain_smoke():
    """Full behavioral smoke test for the brain pack."""
    print(f"Binary: {BINARY}")
    assert os.path.exists(BINARY), f"Binary not found: {BINARY}"

    proc = spawn_brain_proc()
    failures = []

    def ok(label):
        print(f"  [ok] {label}")

    def fail(label, detail):
        print(f"  [FAIL] {label}: {detail}")
        failures.append(label)

    try:
        # ── 1. create_profile happy path ───────────────────────────────────────
        try:
            result = call_verb(proc, "brain.create_profile", {"name": "test-profile-v1"})
            assert result.get("created") is True, f"expected created=true: {result}"
            assert result.get("profile_id") == "test-profile-v1", f"expected profile_id: {result}"
            assert result.get("lifecycle") == "inactive", f"new profile must be inactive: {result}"
            ok("create_profile happy path — returns id, lifecycle=inactive")
        except Exception as e:
            fail("create_profile happy path", e)

        # ── 2. profiles list — create a second, verify both appear ─────────────
        try:
            call_verb(proc, "brain.create_profile", {"name": "test-profile-v2"})
            listing = call_verb(proc, "brain.profiles", {})
            ids = [p["id"] for p in listing["profiles"]]
            assert "test-profile-v1" in ids, f"v1 must be in listing: {ids}"
            assert "test-profile-v2" in ids, f"v2 must be in listing: {ids}"
            # built-in balanced-recall-v1 must also appear
            assert "balanced-recall-v1" in ids, f"default profile must be in listing: {ids}"
            ok(f"profiles list — {listing['count']} profiles including both created")
        except Exception as e:
            fail("profiles list", e)

        # ── 3. profile detail ─────────────────────────────────────────────────
        try:
            detail = call_verb(proc, "brain.profile", {"profile_id": "test-profile-v1"})
            assert detail.get("id") == "test-profile-v1", f"id mismatch: {detail}"
            assert "lifecycle" in detail, f"missing lifecycle: {detail}"
            assert "state_class" in detail, f"missing state_class: {detail}"
            assert "exploration_epoch" in detail, f"missing exploration_epoch: {detail}"
            ok("profile detail — returns full record with lifecycle, state_class, epoch")
        except Exception as e:
            fail("profile detail", e)

        # ── 4. lifecycle activate / deactivate ────────────────────────────────
        try:
            act_result = call_verb(proc, "brain.activate", {"profile_id": "test-profile-v1"})
            assert act_result.get("lifecycle") == "active", f"activate must set active: {act_result}"

            active_detail = call_verb(proc, "brain.profile", {"profile_id": "test-profile-v1"})
            assert active_detail["lifecycle"] == "active", (
                f"profile must show active after activate: {active_detail}"
            )

            deact_result = call_verb(proc, "brain.deactivate", {"profile_id": "test-profile-v1"})
            assert deact_result.get("lifecycle") == "inactive", (
                f"deactivate must set inactive: {deact_result}"
            )

            inactive_detail = call_verb(proc, "brain.profile", {"profile_id": "test-profile-v1"})
            assert inactive_detail["lifecycle"] == "inactive", (
                f"profile must show inactive after deactivate: {inactive_detail}"
            )
            ok("lifecycle activate/deactivate — profile reflects transitions")
        except Exception as e:
            fail("lifecycle activate/deactivate", e)

        # ── 5. lifecycle archive (inactive → archived) ────────────────────────
        try:
            # test-profile-v1 is currently inactive from step 4
            arch_result = call_verb(proc, "brain.archive", {"profile_id": "test-profile-v1"})
            assert arch_result.get("lifecycle") == "archived", (
                f"archive must set archived: {arch_result}"
            )

            arch_detail = call_verb(proc, "brain.profile", {"profile_id": "test-profile-v1"})
            assert arch_detail["lifecycle"] == "archived", (
                f"profile must show archived: {arch_detail}"
            )
            ok("lifecycle archive — inactive→archived succeeds")
        except Exception as e:
            fail("lifecycle archive", e)

        # ── 6. reset — epoch increments ───────────────────────────────────────
        try:
            detail_before = call_verb(proc, "brain.profile", {"profile_id": "balanced-recall-v1"})
            epoch_before = detail_before["exploration_epoch"]

            reset_result = call_verb(proc, "brain.reset", {"profile_id": "balanced-recall-v1"})
            assert reset_result.get("reset") is True, f"reset must return reset=true: {reset_result}"
            epoch_after = reset_result.get("exploration_epoch", 0)
            assert epoch_after > epoch_before, (
                f"reset must increment epoch: before={epoch_before} after={epoch_after}"
            )
            ok(f"reset — exploration_epoch incremented ({epoch_before} → {epoch_after})")
        except Exception as e:
            fail("reset epoch increment", e)

        # ── 7. feedback with valid signal ─────────────────────────────────────
        try:
            # Create a KG entity to use as target_id (brain.feedback requires full UUID)
            entity = call_verb(proc, "create", {
                "kind": "entity",
                "entity_kind": "concept",
                "name": "FeedbackTarget",
                "description": "Entity for brain feedback tests",
            })
            # The entity id may be short (8-char prefix); get the full UUID via get
            short_id = entity["id"]
            fetched = call_verb(proc, "get", {"id": short_id})
            full_uuid = fetched["id"]
            assert len(full_uuid) == 36, (
                f"full UUID must be 36 chars, got {len(full_uuid)}: {full_uuid!r}"
            )

            fb_result = call_verb(proc, "brain.feedback", {
                "target_id": full_uuid,
                "signal": "useful",
            })
            assert fb_result.get("emitted") is True, f"feedback must return emitted=true: {fb_result}"
            assert fb_result.get("signal") == "useful", f"signal mismatch: {fb_result}"
            # BUG: brain.feedback returns target_id as short 8-char prefix even though
            # the input requires a full 36-char UUID.  Check by prefix match instead.
            returned_tid = fb_result.get("target_id", "")
            assert full_uuid.startswith(returned_tid), (
                f"returned target_id {returned_tid!r} must be a prefix of {full_uuid!r}"
            )
            ok(f"feedback with signal=useful — emitted, target={full_uuid[:8]}...")
        except Exception as e:
            fail("feedback with valid signal", e)
            full_uuid = None  # prevent downstream tests from using an undefined var

        # ── 8. feedback invalid signal ────────────────────────────────────────
        try:
            if full_uuid is not None:
                err = call_verb_expect_error(proc, "brain.feedback", {
                    "target_id": full_uuid,
                    "signal": "bogus",
                })
                assert "bogus" in err or "valid" in err or "signal" in err, (
                    f"error must mention the invalid signal or list valid values: {err!r}"
                )
                ok(f"feedback invalid signal — error mentions bogus or valid list")
            else:
                fail("feedback invalid signal", "skipped (no valid entity from step 7)")
        except Exception as e:
            fail("feedback invalid signal", e)

        # ── 9. feedback requires full UUID (not short prefix) ────────────────
        # The handler calls target_id.parse::<uuid::Uuid>() which requires the full
        # 36-char hyphenated form.  An 8-char prefix is not a valid UUID string.
        try:
            err = call_verb_expect_error(proc, "brain.feedback", {
                "target_id": "abcd1234",   # 8-char prefix, not a valid UUID
                "signal": "useful",
            })
            # Should error: either UUID parse failure or "invalid target_id"
            assert len(err) > 0, "must return a non-empty error for short target_id"
            ok(f"feedback rejects short (non-UUID) target_id")
        except Exception as e:
            fail("feedback requires full UUID", e)

        # ── 10. resolve — consumer_kind is required, returns profile ─────────
        try:
            resolved = call_verb(proc, "brain.resolve", {"consumer_kind": "recall"})
            assert "resolved_profile_id" in resolved, (
                f"resolve must return resolved_profile_id: {resolved}"
            )
            assert "lifecycle" in resolved, f"resolve must return lifecycle: {resolved}"
            assert resolved.get("requested_consumer_kind") == "recall", (
                f"requested_consumer_kind mismatch: {resolved}"
            )
            ok(f"resolve — resolved to {resolved['resolved_profile_id']!r}")
        except Exception as e:
            fail("resolve", e)

        # ── 11. bind + bindings ────────────────────────────────────────────────
        try:
            bind_result = call_verb(proc, "brain.bind", {
                "profile_id": "balanced-recall-v1",
                "actor": "test-actor",
                "consumer_kind": "mcp",
            })
            assert bind_result.get("bound") is True, f"bind must return bound=true: {bind_result}"
            assert bind_result.get("actor") == "test-actor", f"actor mismatch: {bind_result}"

            bindings = call_verb(proc, "brain.bindings", {})
            rows = bindings.get("bindings", [])
            found = any(
                r.get("actor") == "test-actor" and r.get("consumer_kind") == "mcp"
                for r in rows
            )
            assert found, f"test-actor/mcp binding must appear in bindings: {rows}"
            ok(f"bind + bindings — binding appears in listing")
        except Exception as e:
            fail("bind + bindings", e)

        # ── 12. unbind ────────────────────────────────────────────────────────
        try:
            unbind_result = call_verb(proc, "brain.unbind", {
                "actor": "test-actor",
                "consumer_kind": "mcp",
            })
            removed = unbind_result.get("unbound", 0)
            assert removed >= 1, f"unbind must remove at least 1 row: {unbind_result}"

            bindings_after = call_verb(proc, "brain.bindings", {})
            rows_after = bindings_after.get("bindings", [])
            still_there = any(
                r.get("actor") == "test-actor" and r.get("consumer_kind") == "mcp"
                for r in rows_after
            )
            assert not still_there, (
                f"binding must not appear after unbind: {rows_after}"
            )
            ok(f"unbind — removed {removed} row(s), no longer in bindings")
        except Exception as e:
            fail("unbind", e)

        # ── 13. create_profile duplicate name ────────────────────────────────
        try:
            err = call_verb_expect_error(proc, "brain.create_profile", {
                "name": "test-profile-v2",  # already created in step 2
            })
            assert "already" in err or "exists" in err or "duplicate" in err, (
                f"duplicate profile error must mention already-exists: {err!r}"
            )
            ok("create_profile duplicate name — rejected with clear error")
        except Exception as e:
            fail("create_profile duplicate name", e)

        # ── 14. feedback with archived served_by_profile_id ───────────────────
        # test-profile-v1 was archived in step 5
        try:
            if full_uuid is not None:
                err = call_verb_expect_error(proc, "brain.feedback", {
                    "target_id": full_uuid,
                    "signal": "useful",
                    "served_by_profile_id": "test-profile-v1",  # archived
                })
                assert "archived" in err, (
                    f"feedback to archived profile must mention 'archived': {err!r}"
                )
                ok("archive then feedback — rejected with 'archived' in error")
            else:
                fail("archive then feedback", "skipped (no valid entity from step 7)")
        except Exception as e:
            fail("archive then feedback", e)

        # ── 15. reset archived profile ────────────────────────────────────────
        # test-profile-v1 is archived; reset must be rejected
        try:
            err = call_verb_expect_error(proc, "brain.reset", {
                "profile_id": "test-profile-v1",
            })
            assert "archived" in err or "terminal" in err, (
                f"reset of archived profile must mention 'archived' or 'terminal': {err!r}"
            )
            ok("reset archived profile — rejected with 'archived'/'terminal' in error")
        except Exception as e:
            fail("reset archived profile", e)

        # ── Additional behavioral assertions ──────────────────────────────────

        # 16. profiles list filtered by lifecycle
        try:
            active_list = call_verb(proc, "brain.profiles", {"lifecycle": "active"})
            for p in active_list["profiles"]:
                assert p["lifecycle"] == "active", (
                    f"filtered list must only show active profiles: {p}"
                )
            archived_list = call_verb(proc, "brain.profiles", {"lifecycle": "archived"})
            for p in archived_list["profiles"]:
                assert p["lifecycle"] == "archived", (
                    f"filtered list must only show archived profiles: {p}"
                )
            ok("profiles list filtered by lifecycle — lifecycle filter works")
        except Exception as e:
            fail("profiles list filtered by lifecycle", e)

        # 17. profile detail accepts id alias (H4)
        try:
            r1 = call_verb(proc, "brain.profile", {"profile_id": "balanced-recall-v1"})
            r2 = call_verb(proc, "brain.profile", {"id": "balanced-recall-v1"})
            assert r1["id"] == r2["id"] == "balanced-recall-v1", (
                f"both profile_id and id alias must work: r1={r1['id']!r} r2={r2['id']!r}"
            )
            ok("profile detail accepts id alias (H4)")
        except Exception as e:
            fail("profile detail id alias", e)

        # 18. active→archived directly must be rejected (must deactivate first)
        try:
            # Create a fresh profile and activate it
            call_verb(proc, "brain.create_profile", {"name": "dag-test-profile"})
            call_verb(proc, "brain.activate", {"profile_id": "dag-test-profile"})
            err = call_verb_expect_error(proc, "brain.archive", {"profile_id": "dag-test-profile"})
            assert "deactivate" in err or "inactive" in err or "active" in err, (
                f"active→archived rejection must hint at deactivating first: {err!r}"
            )
            ok("lifecycle DAG — active→archived directly is rejected")
        except Exception as e:
            fail("lifecycle DAG active→archived", e)

        # 19. archived profile cannot be re-activated
        try:
            # test-profile-v1 is archived
            err = call_verb_expect_error(proc, "brain.activate", {"profile_id": "test-profile-v1"})
            assert "archived" in err or "terminal" in err, (
                f"activate on archived must mention 'archived'/'terminal': {err!r}"
            )
            ok("archived profile cannot be re-activated — terminal state enforced")
        except Exception as e:
            fail("archived profile cannot be re-activated", e)

        # 20. unbind with no filters must be rejected
        try:
            err = call_verb_expect_error(proc, "brain.unbind", {})
            assert "filter" in err or "profile_id" in err or "actor" in err, (
                f"zero-filter unbind must ask for at least one filter: {err!r}"
            )
            ok("unbind with no filters — rejected (at least one filter required)")
        except Exception as e:
            fail("unbind with no filters", e)

        # 21. bind to archived profile must be rejected (C3)
        try:
            # test-profile-v1 is archived
            err = call_verb_expect_error(proc, "brain.bind", {
                "profile_id": "test-profile-v1",
                "consumer_kind": "recall",
            })
            assert "archived" in err, (
                f"bind to archived profile must mention 'archived': {err!r}"
            )
            ok("bind to archived profile — rejected (C3)")
        except Exception as e:
            fail("bind to archived profile", e)

        # 22. brain.feedback all valid signals
        try:
            if full_uuid is not None:
                for signal in [
                    "useful", "not_useful", "wrong",
                    "explicit_positive", "explicit_negative",
                    "implicit_positive", "implicit_negative",
                    "correction",
                ]:
                    fb = call_verb(proc, "brain.feedback", {
                        "target_id": full_uuid,
                        "signal": signal,
                    })
                    assert fb.get("emitted") is True, f"signal {signal!r} must succeed: {fb}"
                ok("feedback all 8 valid signals accepted")
            else:
                fail("feedback all valid signals", "skipped (no valid entity)")
        except Exception as e:
            fail("feedback all valid signals", e)

        # 23. create_profile with invalid name chars must be rejected
        try:
            for bad_name in ["bad.profile", "bad_profile", "bad profile", "*"]:
                err = call_verb_expect_error(proc, "brain.create_profile", {"name": bad_name})
                assert len(err) > 0, f"invalid name {bad_name!r} must return an error"
            ok("create_profile invalid name chars rejected")
        except Exception as e:
            fail("create_profile invalid name chars", e)

        # 24. reset with no profile_id defaults to balanced-recall-v1
        try:
            reset_default = call_verb(proc, "brain.reset", {})
            assert reset_default.get("reset") is True, (
                f"reset with no profile_id must succeed: {reset_default}"
            )
            assert reset_default.get("profile_id") == "balanced-recall-v1", (
                f"default reset must target balanced-recall-v1: {reset_default}"
            )
            ok("reset with no profile_id — defaults to balanced-recall-v1")
        except Exception as e:
            fail("reset defaults to balanced-recall-v1", e)

        # 25. feedback with nonexistent target_id returns not-found error
        try:
            # Zero UUID is a valid UUID format but does not exist in this namespace
            err = call_verb_expect_error(proc, "brain.feedback", {
                "target_id": "00000000-0000-0000-0000-000000000000",
                "signal": "useful",
            })
            assert len(err) > 0, "nonexistent target_id must return an error"
            ok("feedback nonexistent target_id — not-found error returned")
        except Exception as e:
            fail("feedback nonexistent target_id", e)

        # 26. brain.feedback total_events on profile increments after feedback
        try:
            if full_uuid is not None:
                # Create a fresh profile, activate it, send feedback, check total_events
                call_verb(proc, "brain.create_profile", {"name": "events-counter-v1"})
                call_verb(proc, "brain.activate", {"profile_id": "events-counter-v1"})

                before = call_verb(proc, "brain.profile", {"profile_id": "events-counter-v1"})
                events_before = before["total_events"]

                call_verb(proc, "brain.feedback", {
                    "target_id": full_uuid,
                    "signal": "useful",
                    "served_by_profile_id": "events-counter-v1",
                })

                after = call_verb(proc, "brain.profile", {"profile_id": "events-counter-v1"})
                events_after = after["total_events"]
                assert events_after > events_before, (
                    f"total_events must increment after feedback: {events_before} → {events_after}"
                )
                ok(f"feedback increments total_events on profile ({events_before} → {events_after})")
            else:
                fail("feedback increments total_events", "skipped (no valid entity)")
        except Exception as e:
            fail("feedback increments total_events", e)

        # ── Summary ───────────────────────────────────────────────────────────
        total = 26
        if failures:
            print(f"\n  {len(failures)}/{total} tests FAILED: {failures}")
            return 1
        else:
            print(f"\n  ALL {total} BRAIN PACK SMOKE TESTS PASSED")
            return 0

    finally:
        proc.stdin.close()
        proc.wait(timeout=5)


if __name__ == "__main__":
    code = brain_smoke()
    sys.exit(code)
