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
from .models import Edge, EdgeRelation, Entity, Note, OpResult, Page
from .ops import encode, op
from .transport import Session, SocketTransport, Transport

__all__ = [
    "Khive",
    "Entity",
    "Note",
    "Edge",
    "EdgeRelation",
    "Page",
    "OpResult",
    "op",
    "encode",
    "Transport",
    "SocketTransport",
    "Session",
    "KhiveError",
    "TransportError",
    "FrameTooLarge",
    "ProtocolMismatch",
    "ConfigMismatch",
    "RequestRejected",
    "OperationError",
    "BatchError",
]

__version__ = "0.1.0"
