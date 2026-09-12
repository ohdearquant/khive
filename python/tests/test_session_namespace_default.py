"""A session's namespace is the default for its ops, not just a frame field.

The constructor argument used to default to "local" and reach only the request
frame. The registry does not read the frame as an op argument, so a session
built with a namespace sent every op unscoped and answered about the caller's
own namespace instead, with no error anywhere. These arms pin the argument to
the meaning it reads as, and the frame arm pins what did NOT change.
"""

from __future__ import annotations

import copy
import json

from khive import Transport
from khive.transport import Session

NOTE_ID = "00000000-0000-4000-8000-000000000001"


class RecordingTransport(Transport):
    """Records every dispatched frame and answers one successful remember."""

    def __init__(self) -> None:
        self.dispatches: list[dict] = []

    def round_trip(self, frame, timeout):
        response = {"ok": True, "served_config_id": "test-config"}
        if not frame.get("metrics_only"):
            self.dispatches.append(copy.deepcopy(frame))
            response["result"] = json.dumps(
                {
                    "results": [
                        {
                            "ok": True,
                            "tool": "memory.remember",
                            "result": {"id": NOTE_ID},
                        }
                    ]
                }
            )
        return response


def dispatch(session: Session, **kwargs) -> dict:
    """Return the dispatched op's ARGS, parsed.

    The ops payload is JSON, not DSL source, so a substring check for
    `namespace="acme"` never matches and a check for `namespace=` never
    matches either -- which would make the absence arm below pass without
    testing anything. The structural question gets a parse.
    """
    transport = session.transport
    session.remember("a marker phrase", **kwargs)
    assert transport.dispatches, "no frame was dispatched; the arm proves nothing"
    ops = json.loads(transport.dispatches[-1]["ops"])
    assert len(ops) == 1 and ops[0]["tool"] == "memory.remember"
    return ops[0]["args"]


def session(**kwargs) -> Session:
    return Session(transport=RecordingTransport(), **kwargs)


def test_session_namespace_reaches_the_op_not_only_the_frame():
    args = dispatch(session(namespace="acme"))
    assert args["namespace"] == "acme"


def test_an_explicit_op_namespace_beats_the_session_default():
    args = dispatch(session(namespace="acme"), namespace="other")
    assert args["namespace"] == "other"


def test_a_session_naming_no_namespace_sends_no_namespace_argument():
    # The control for the two arms above: without this, an implementation that
    # hard-coded "acme" would pass both of them.
    args = dispatch(session())
    assert "namespace" not in args


def test_the_frame_still_carries_a_string_for_an_unset_session():
    # The wire contract is unchanged by moving the default to None. The frame's
    # namespace is an identity field, and it was a string before this change.
    s = session()
    dispatch(s)
    assert s.transport.dispatches[-1]["namespace"] == "local"


def test_a_named_session_puts_its_namespace_on_both_frame_and_op():
    s = session(namespace="acme")
    args = dispatch(s)
    assert s.transport.dispatches[-1]["namespace"] == "acme"
    assert args["namespace"] == "acme"
