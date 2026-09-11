"""Message-pair acceptance through an isolated, freshly built debug daemon.

Set KKERNEL to this checkout's debug binary. The scratch_daemon fixture owns
the database and sockets; acceptance must report zero skips. Run the older-server
control separately with KHIVE_TEST_LEGACY_COMM=1 and a pre-feature KKERNEL.
"""

from __future__ import annotations

import concurrent.futures
import os
import sqlite3
import uuid

import pytest

from khive import Session, SocketTransport, Transport, encode, op
from khive.errors import TransportError


@pytest.fixture
def parties(scratch_daemon):
    suffix = uuid.uuid4().hex
    namespace = f"mail-{suffix}"
    sender_id, recipient_id = f"sender:{suffix}", f"recipient:{suffix}"

    def client(actor):
        return Session(SocketTransport(scratch_daemon["socket"]), actor_id=actor)

    return client(sender_id), client(recipient_id), namespace, recipient_id


def one(client, verb, **args):
    outcome = client.request(encode([op(verb, **args)]))[0]
    assert outcome["ok"], outcome
    return outcome["result"]


def ok(outcome):
    assert outcome["ok"], outcome
    return outcome["result"]


def rows(scratch_daemon, namespace):
    # Snapshot all message domain rows, including read state and deleted rows.
    with sqlite3.connect(f"file:{scratch_daemon['root'] / 'scratch.db'}?mode=ro", uri=True) as db:
        return db.execute(
            "SELECT id, namespace, content, properties, key, created_at, updated_at, deleted_at "
            "FROM notes WHERE kind='message' AND namespace=? ORDER BY id",
            (namespace,),
        ).fetchall()


def same_pair(first, replay):
    assert first["replayed"] is False and replay["replayed"] is True
    for field in ("full_id", "recipient_id", "idempotency_key", "sent_at", "thread_id"):
        assert replay[field] == first[field]
        assert replay[field] is not None


def conflict(outcome, original, key):
    assert outcome["ok"] is False, outcome
    error = outcome["error"]
    assert error["kind"] == "conflict"
    assert error["domain_disposition"] == "not_committed"
    assert error["details"]["reason"] == "key_conflict"
    assert error["details"]["existing_id"] == original["full_id"]
    assert error["details"]["key"] == key


def test_send_replay_conflict_and_readback(scratch_daemon, parties):
    sender, recipient, namespace, to = parties
    args = dict(
        to=to,
        content="exact body",
        subject="subject",
        tags=["t", "t"],
        idempotency_key="send-key",
        namespace=namespace,
    )
    first = ok(sender.send(**args))
    before = rows(scratch_daemon, namespace)
    assert len(before) == 2 and sum(row[4] is not None for row in before) == 1
    same_pair(first, ok(sender.send(**args)))
    assert rows(scratch_daemon, namespace) == before
    conflict(sender.send(**(args | {"content": "changed"})), first, "send-key")
    assert rows(scratch_daemon, namespace) == before
    for client, box, expected_id in (
        (sender, "sent", first["full_id"]),
        (recipient, "inbox", first["recipient_id"]),
    ):
        filters = {"status": "all"} if box == "inbox" else {}
        result = one(client, "comm.inbox", box=box, namespace=namespace, **filters)
        assert result["count"] == 1
        assert result["messages"][0]["full_id"] == expected_id
        assert result["messages"][0]["properties"]["idempotency_key"] == "send-key"
        projected = one(
            client,
            "comm.inbox",
            box=box,
            namespace=namespace,
            fields=["full_id", "idempotency_key"],
            **filters,
        )
        assert projected["messages"][0]["idempotency_key"] == "send-key"
        thread = one(client, "comm.thread", id=first["thread_id"], namespace=namespace)
        assert all(row["properties"]["idempotency_key"] == "send-key" for row in thread["messages"])
    read = one(recipient, "comm.read", id=first["recipient_id"], namespace=namespace)
    assert read["properties"]["idempotency_key"] == "send-key"
    refused = sender.request(encode([op("comm.read", id=first["full_id"], namespace=namespace)]))[0]
    assert refused["ok"] is False
    after_read = rows(scratch_daemon, namespace)
    same_pair(first, ok(sender.send(**args)))
    assert rows(scratch_daemon, namespace) == after_read


def test_reply_replay_conflict_preserves_parent(scratch_daemon, parties):
    sender, recipient, namespace, to = parties
    parent = ok(sender.send(to, "question", subject="topic", namespace=namespace))
    inbox = one(recipient, "comm.inbox", namespace=namespace)
    parent_id = inbox["messages"][0]["full_id"]
    args = dict(id=parent_id, content="answer", idempotency_key="reply-key", namespace=namespace)
    first = ok(recipient.reply(**args))
    assert first["thread_id"] == parent["thread_id"]
    before = rows(scratch_daemon, namespace)
    assert len(before) == 4
    replay = ok(recipient.reply(**(args | {"id": parent_id[:8]})))
    same_pair(first, replay)
    assert replay["marked_read"] is None
    assert rows(scratch_daemon, namespace) == before
    conflict(recipient.reply(**(args | {"content": "different"})), first, "reply-key")
    assert rows(scratch_daemon, namespace) == before


class LoseFirstAcknowledgement(Transport):
    def __init__(self, socket):
        self.delegate = SocketTransport(socket)
        self.receipt = None
        self.dispatches = 0

    def round_trip(self, frame, timeout):
        response = self.delegate.round_trip(frame, timeout)
        if not frame.get("metrics_only"):
            self.dispatches += 1
            self.receipt = response
            raise TransportError("test discarded a completed daemon response")
        return response


def test_lost_ack_requires_explicit_replay_and_keeps_original_ids(scratch_daemon, parties):
    sender, _, namespace, to = parties
    lossy = LoseFirstAcknowledgement(scratch_daemon["socket"])
    first_client = Session(lossy, actor_id=sender.actor_id)
    args = dict(to=to, content="lost acknowledgement", idempotency_key="lost", namespace=namespace)
    with pytest.raises(TransportError, match="discarded"):
        first_client.send(**args)
    assert lossy.dispatches == 1
    before = rows(scratch_daemon, namespace)
    assert len(before) == 2
    replay = ok(sender.send(**args))
    assert replay["replayed"] is True
    assert {replay["full_id"], replay["recipient_id"]} == {row[0] for row in before}
    assert rows(scratch_daemon, namespace) == before


def test_concurrent_clients_create_one_pair(scratch_daemon, parties):
    sender, _, namespace, to = parties

    def send(_):
        client = Session(SocketTransport(scratch_daemon["socket"]), actor_id=sender.actor_id)
        return ok(client.send(to, "race", idempotency_key="race", namespace=namespace))

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as workers:
        results = list(workers.map(send, range(2)))
    results.sort(key=lambda result: result["replayed"])
    same_pair(*results)
    assert len(rows(scratch_daemon, namespace)) == 2


@pytest.mark.skipif(
    os.environ.get("KHIVE_TEST_LEGACY_COMM") != "1",
    reason="set KHIVE_TEST_LEGACY_COMM=1 and KKERNEL to a server predating keyed comm",
)
@pytest.mark.parametrize("method", ["send", "reply"])
def test_older_server_refuses_key_without_unkeyed_fallback(scratch_daemon, parties, method):
    """Run separately against a pinned pre-amendment server, using the new client."""
    sender, recipient, namespace, to = parties
    parent = ok(sender.send(to, "legacy parent", namespace=namespace))
    before = rows(scratch_daemon, namespace)
    assert len(before) == 2
    client, args = (
        (sender, dict(to=to, content="must refuse"))
        if method == "send"
        else (recipient, dict(id=parent["full_id"], content="must refuse"))
    )
    result = getattr(client, method)(**args, namespace=namespace, idempotency_key="legacy-key")
    assert result["ok"] is False, result
    error = result["error"]
    assert error["kind"] == "runtime_error", error
    assert "bad params: unknown field `idempotency_key`" in error["message"], error
    assert "idempotency_key" in error["message"], error
    # The old generic InvalidInput adapter conservatively says unknown. Do not
    # invent a stronger wire disposition; the database assertion proves refusal.
    assert error["domain_disposition"] == "unknown", error
    assert rows(scratch_daemon, namespace) == before
    # An ordinary call still works: the refusal was specifically the unsupported key.
    ok(getattr(client, method)(**args, namespace=namespace))
    assert len(rows(scratch_daemon, namespace)) == 4
