#!/usr/bin/env python3
"""Smoke + behavioral tests for the khive-mcp comm pack over MCP stdio.

Spawns the binary with an in-memory DB and the comm pack loaded, then
exercises every verb (send, inbox, read, reply, thread) via the `request`
DSL over JSON-RPC.

Usage:
    uv run python tests/smoke_comm.py
    # or: python3 tests/smoke_comm.py
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
    """Call a verb and expect a per-op error. Returns the error string, raises if verb succeeds."""
    ops = json.dumps([{"tool": name, "args": args}])
    body = _call_request_raw(proc, ops)
    if body is None:
        raise AssertionError(f"{name} returned empty body but success was expected to fail")
    results = body.get("results") or []
    if not results:
        raise AssertionError(f"{name} returned no results: {body}")
    first = results[0]
    if first.get("ok", False):
        raise AssertionError(
            f"{name} succeeded but an error was expected; result={first.get('result')}"
        )
    return first.get("error", "")


def init_proc(proc, client_name="comm-smoke"):
    """Run MCP initialization handshake."""
    send(proc, "initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": client_name, "version": "0.1.0"},
    })
    recv(proc)
    notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
    proc.stdin.write((json.dumps(notify) + "\n").encode())
    proc.stdin.flush()


def spawn():
    """Spawn the MCP binary with kg + comm packs loaded."""
    return subprocess.Popen(
        [BINARY, "mcp", "--db", ":memory:", "--no-embed", "--log", "error",
         "--pack", "kg", "--pack", "comm"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


# ── helpers ───────────────────────────────────────────────────────────────────

def get_inbound_from_inbox(proc):
    """Send a message to self and return the inbound message dict from inbox."""
    call_verb(proc, "comm.send", {"to": "local", "content": "setup message"})
    inbox_result = call_verb(proc, "comm.inbox", {"status": "unread", "limit": 1})
    msgs = inbox_result.get("messages", [])
    assert msgs, "Expected at least 1 inbound message in inbox"
    return msgs[0]


# ── tests ─────────────────────────────────────────────────────────────────────

def test_happy_path(proc):
    """send → inbox shows it → read marks it → inbox(status=read) shows it, unread doesn't."""
    # Send a message to self
    sent = call_verb(proc, "comm.send", {"to": "local", "content": "hello from smoke test"})
    assert sent.get("id"), f"send must return id: {sent}"
    assert sent.get("full_id"), f"send must return full_id: {sent}"
    assert len(sent["id"]) == 8, f"short id must be 8 chars: {sent['id']}"
    assert len(sent["full_id"]) == 36, f"full_id must be 36 chars: {sent['full_id']}"

    # inbox(status=unread) should show the inbound copy
    inbox = call_verb(proc, "comm.inbox", {"status": "unread"})
    assert inbox.get("count", 0) >= 1, f"inbox must show unread message: {inbox}"
    msgs = inbox["messages"]
    assert any(m.get("content") == "hello from smoke test" for m in msgs), (
        f"inbox must contain sent message: {msgs}"
    )

    # Get the inbound message full_id
    inbound_msg = next(
        m for m in msgs if m.get("content") == "hello from smoke test"
    )
    inbound_full_id = inbound_msg["full_id"]
    assert inbound_full_id != sent["full_id"], (
        "inbound copy must have a different id from the outbound copy"
    )

    # read() marks it as read
    read_result = call_verb(proc, "comm.read", {"id": inbound_full_id})
    assert read_result.get("read") is True, f"read must return read=true: {read_result}"
    assert read_result.get("full_id") == inbound_full_id, "read must return same full_id"

    # inbox(status=read) must show it
    read_inbox = call_verb(proc, "comm.inbox", {"status": "read"})
    read_ids = [m["full_id"] for m in read_inbox.get("messages", [])]
    assert inbound_full_id in read_ids, (
        f"inbox(status=read) must include the read message; got {read_ids}"
    )

    # inbox(status=unread) must NOT show it
    unread_inbox = call_verb(proc, "comm.inbox", {"status": "unread"})
    unread_ids = [m["full_id"] for m in unread_inbox.get("messages", [])]
    assert inbound_full_id not in unread_ids, (
        f"inbox(status=unread) must exclude the read message; got {unread_ids}"
    )
    print("  [ok] happy path: send → inbox → read → status filter")


def test_reply_and_thread(proc):
    """send → reply → thread returns 2+ messages in chronological order with thread_id."""
    # Send root message
    root = call_verb(proc, "comm.send", {"to": "local", "content": "root message"})
    root_full_id = root["full_id"]
    root_short_id = root["id"]  # 8-char

    # Reply to it
    reply = call_verb(proc, "comm.reply", {"id": root_full_id, "content": "first reply"})
    assert reply.get("id"), f"reply must return id: {reply}"

    # The presentation layer (Agent mode) shortens all *_id fields to 8 chars.
    # thread_id ends with "_id" → it gets shortened. So compare against the
    # short prefix of the root's full_id.
    # BUG NOTE: this is expected behavior by design (ADR-045 Agent mode presentation),
    # but callers that need the full thread_id for chaining should use verbose mode.
    reply_thread_id = reply.get("thread_id", "")
    assert reply_thread_id, f"reply must have a thread_id: {reply}"
    # Agent mode shortens UUIDs with _id suffix to 8 chars
    assert root_full_id.startswith(reply_thread_id), (
        f"reply thread_id must be a prefix of root full_id; "
        f"root_full_id={root_full_id!r} reply_thread_id={reply_thread_id!r}"
    )
    reply_full_id = reply["full_id"]
    assert len(reply_full_id) == 36, f"reply full_id must be 36 chars: {reply_full_id}"

    # Thread must return at least root + reply
    thread = call_verb(proc, "comm.thread", {"id": root_full_id})
    assert thread.get("count", 0) >= 2, (
        f"thread must return root + reply (>=2); got count={thread.get('count')}"
    )
    # thread_id in thread response is also shortened by Agent mode
    thread_tid = thread.get("thread_id", "")
    assert thread_tid, f"thread must return thread_id: {thread}"
    assert root_full_id.startswith(thread_tid), (
        f"thread thread_id must be a prefix of root full_id; "
        f"root_full_id={root_full_id!r} thread_tid={thread_tid!r}"
    )

    # Messages must be in chronological order (oldest first). Agent mode
    # renders created_at as relative strings ("1s ago", "0s ago"), which sort
    # lexicographically in the WRONG direction for oldest-first output — a
    # correctly ordered thread whose root crosses a 1-second boundary fails a
    # sorted() comparison. Assert creation order by content instead: every
    # copy of the root (dual-write emits outbound + inbound) must precede
    # every copy of the reply.
    msgs = thread["messages"]
    contents = [(m.get("content") or m.get("preview") or "") for m in msgs]
    root_positions = [i for i, c in enumerate(contents) if "root message" in c]
    reply_positions = [i for i, c in enumerate(contents) if "first reply" in c]
    assert root_positions and reply_positions, (
        f"thread must contain both root and reply; got {contents}"
    )
    assert max(root_positions) < min(reply_positions), (
        f"thread messages must be in chronological order (root before reply); got {contents}"
    )
    print("  [ok] reply + thread: chronological order, correct thread_id")


def test_subject_threading(proc):
    """send with subject → reply prepends 'Re: ' → already 'Re: ' doesn't double-prepend."""
    # Send with subject
    root = call_verb(proc, "comm.send", {
        "to": "local",
        "content": "about the project",
        "subject": "Project Update",
    })
    root_full_id = root["full_id"]

    # Reply — subject should get "Re: " prepended
    reply1 = call_verb(proc, "comm.reply", {"id": root_full_id, "content": "got it"})
    assert reply1.get("subject") == "Re: Project Update", (
        f"reply subject must be 'Re: Project Update'; got {reply1.get('subject')!r}"
    )

    # Reply to reply — already "Re: ", should NOT double-prepend
    reply1_full_id = reply1["full_id"]
    reply2 = call_verb(proc, "comm.reply", {"id": reply1_full_id, "content": "thanks"})
    assert reply2.get("subject") == "Re: Project Update", (
        f"reply to reply must NOT double-prepend 'Re: '; got {reply2.get('subject')!r}"
    )
    print("  [ok] subject threading: Re: prepend, no double-prepend")


def test_empty_content_rejection(proc):
    """send with empty content → error."""
    err = call_verb_expect_error(proc, "comm.send", {"to": "local", "content": ""})
    assert err, "expected a non-empty error message"
    # The handler checks: `content.trim().is_empty()`
    assert "content" in err.lower() or "empty" in err.lower(), (
        f"error must mention content or empty: {err!r}"
    )
    print("  [ok] empty content rejection")


def test_empty_to_rejection(proc):
    """send with empty to → error."""
    err = call_verb_expect_error(proc, "comm.send", {"to": "", "content": "hello"})
    assert err, "expected a non-empty error message"
    assert "to" in err.lower() or "empty" in err.lower(), (
        f"error must mention 'to' or empty: {err!r}"
    )
    print("  [ok] empty to rejection")


def test_read_outbound_rejection(proc):
    """send → try to read the outbound message id → error mentioning 'outbound'."""
    sent = call_verb(proc, "comm.send", {"to": "local", "content": "outbound test"})
    outbound_full_id = sent["full_id"]

    err = call_verb_expect_error(proc, "comm.read", {"id": outbound_full_id})
    assert err, "expected a non-empty error message"
    assert "outbound" in err.lower() or "direction" in err.lower(), (
        f"error must mention 'outbound' or 'direction': {err!r}"
    )
    print("  [ok] read outbound rejection: error mentions outbound/direction")


def test_actor_addressed_send(proc):
    """send with to='other-ns' succeeds as actor-addressed delivery (ADR-057).

    ADR-057 reinterpreted the `to` field as an actor label within the caller's
    namespace. Cross-namespace denial was removed: both copies land in the
    caller's namespace and the actor label is stored in `to_actor`. Sends to
    any non-empty actor label must succeed.
    """
    result = call_verb(proc, "comm.send", {
        "to": "other-ns",
        "content": "actor-addressed message",
    })
    assert result.get("id"), f"send must return id: {result}"
    assert result.get("full_id"), f"send must return full_id: {result}"
    assert result.get("to") == "other-ns", (
        f"to field must preserve the actor label: {result}"
    )
    print("  [ok] actor-addressed send: to='other-ns' accepted (ADR-057)")


def test_inbox_status_filter(proc):
    """Create 2 messages, read one, verify status filters work correctly."""
    # Send 2 messages
    call_verb(proc, "comm.send", {"to": "local", "content": "filter test message 1"})
    call_verb(proc, "comm.send", {"to": "local", "content": "filter test message 2"})

    # Get both from inbox
    all_inbox = call_verb(proc, "comm.inbox", {"status": "all", "limit": 200})
    # Filter to just the ones we sent (content match)
    our_msgs = [
        m for m in all_inbox.get("messages", [])
        if m.get("content") in ("filter test message 1", "filter test message 2")
    ]
    assert len(our_msgs) >= 2, (
        f"Expected at least 2 inbound messages from our sends; got {len(our_msgs)}"
    )

    # Read one of them
    msg_to_read = our_msgs[0]
    call_verb(proc, "comm.read", {"id": msg_to_read["full_id"]})
    read_id = msg_to_read["full_id"]
    unread_id = our_msgs[1]["full_id"]

    # inbox(status=all) must include both
    all_after = call_verb(proc, "comm.inbox", {"status": "all", "limit": 200})
    all_ids = [m["full_id"] for m in all_after.get("messages", [])]
    assert read_id in all_ids, f"inbox(all) must contain read message {read_id}"
    assert unread_id in all_ids, f"inbox(all) must contain unread message {unread_id}"

    # inbox(status=unread) must contain the unread one and not the read one
    unread_after = call_verb(proc, "comm.inbox", {"status": "unread", "limit": 200})
    unread_ids = [m["full_id"] for m in unread_after.get("messages", [])]
    assert unread_id in unread_ids, (
        f"inbox(unread) must contain unread message {unread_id}"
    )
    assert read_id not in unread_ids, (
        f"inbox(unread) must NOT contain read message {read_id}"
    )

    # inbox(status=read) must contain the read one and not the unread one
    read_after = call_verb(proc, "comm.inbox", {"status": "read", "limit": 200})
    read_ids = [m["full_id"] for m in read_after.get("messages", [])]
    assert read_id in read_ids, (
        f"inbox(read) must contain read message {read_id}"
    )
    assert unread_id not in read_ids, (
        f"inbox(read) must NOT contain unread message {unread_id}"
    )
    print("  [ok] inbox status filter: all/unread/read segregate correctly")


def test_reply_to_non_message_note(proc):
    """Create a regular observation note via KG, then try comm.reply on that note → error mentioning 'message'."""
    # Create a non-message note using KG verb
    obs = call_verb(proc, "create", {
        "kind": "note",
        "note_kind": "observation",
        "content": "this is a KG observation note, not a message",
    })
    obs_full_id = obs.get("full_id") or obs.get("id")
    assert obs_full_id, f"create observation must return an id: {obs}"

    # If we got a short id, we need full UUID for reply
    # The KG create verb may return short id; use full_id field if present
    if len(obs_full_id) == 8:
        # Use the full_id field if available
        obs_full_id = obs.get("full_id", obs_full_id)
        # If still short, list notes to find the full UUID
        if len(obs_full_id) == 8:
            # Fall back: list observations to get full UUID
            notes_list = call_verb(proc, "list", {
                "kind": "note",
                "note_kind": "observation",
                "limit": 10,
            })
            obs_note = next(
                (n for n in notes_list
                 if n.get("content") == "this is a KG observation note, not a message"),
                None,
            )
            assert obs_note, "Could not find the observation note"
            obs_full_id = obs_note.get("full_id") or obs_note.get("id")

    err = call_verb_expect_error(proc, "comm.reply", {
        "id": obs_full_id,
        "content": "trying to reply to non-message",
    })
    assert err, "expected a non-empty error message"
    assert "message" in err.lower() or "kind" in err.lower(), (
        f"error must mention 'message' or 'kind': {err!r}"
    )
    print("  [ok] reply to non-message note: error mentions message/kind")


def test_thread_nonexistent_id(proc):
    """comm.thread with a random UUID → error 'not found'."""
    phantom_id = str(uuid.uuid4())
    err = call_verb_expect_error(proc, "comm.thread", {"id": phantom_id})
    assert err, "expected a non-empty error message"
    assert "not found" in err.lower() or "notfound" in err.lower(), (
        f"error must mention 'not found': {err!r}"
    )
    print("  [ok] thread on nonexistent id: not found error")


# ── main ──────────────────────────────────────────────────────────────────────

def main():
    print(f"Binary: {BINARY}")
    assert os.path.exists(BINARY), f"Binary not found: {BINARY}"

    failed = 0
    tests = [
        test_happy_path,
        test_reply_and_thread,
        test_subject_threading,
        test_empty_content_rejection,
        test_empty_to_rejection,
        test_read_outbound_rejection,
        test_actor_addressed_send,
        test_inbox_status_filter,
        test_reply_to_non_message_note,
        test_thread_nonexistent_id,
    ]

    for test_fn in tests:
        proc = spawn()
        try:
            init_proc(proc)
            test_fn(proc)
        except Exception as e:
            print(f"  [FAIL] {test_fn.__name__}: {e}")
            failed += 1
        finally:
            proc.stdin.close()
            proc.wait(timeout=5)

    if failed == 0:
        print(f"\n  ALL {len(tests)} COMM PACK SMOKE TESTS PASSED")
        return 0
    else:
        print(f"\n  {failed}/{len(tests)} COMM PACK SMOKE TESTS FAILED")
        return 1


if __name__ == "__main__":
    sys.exit(main())
