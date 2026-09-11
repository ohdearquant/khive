"""khive-py — Python client for the khive knowledge-graph database.

Talks to the khived daemon over its native Unix-socket wire; never opens
the database file. See `client.Khive` for the API and `transport` for the
wire contract.
"""

from .client import Khive
from .errors import (
    BatchError,
    ConfigMismatch,
    FrameTooLarge,
    KhiveError,
    OperationError,
    ProtocolMismatch,
    RequestRejected,
    TransportError,
)
from .models import (
    Edge,
    EdgeRelation,
    Entity,
    Incidence,
    Note,
    OpError,
    OpResult,
    Page,
    RecallHit,
    RecallOutcome,
)
from .ops import encode, op
from .transport import Session, SocketTransport, Transport

__all__ = [
    "BatchError",
    "ConfigMismatch",
    "Edge",
    "EdgeRelation",
    "Entity",
    "FrameTooLarge",
    "Incidence",
    "Khive",
    "KhiveError",
    "Note",
    "OpError",
    "OpResult",
    "OperationError",
    "Page",
    "ProtocolMismatch",
    "RecallHit",
    "RecallOutcome",
    "RequestRejected",
    "Session",
    "SocketTransport",
    "Transport",
    "TransportError",
    "encode",
    "op",
]

__version__ = "0.1.1"
