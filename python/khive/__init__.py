"""khive-py — Python client for the khive knowledge-graph database.

Talks to the khived daemon over its native Unix-socket wire; never opens
the database file. See `client.Khive` for the API and `transport` for the
wire contract.
"""

from .client import Khive
from .cloud import cloud
from .errors import (
    AuthError,
    BadRequest,
    BatchError,
    ConfigMismatch,
    FrameTooLarge,
    HttpError,
    KhiveError,
    OperationError,
    ProtocolMismatch,
    RateLimited,
    RequestRejected,
    ServerError,
    TransportError,
    http_op_error_code,
)
from .models import (
    Attachment,
    Edge,
    EdgeRelation,
    Embedding,
    Entity,
    Incidence,
    Note,
    OpResult,
    Page,
)
from .ops import encode, op
from .transport import AsyncHttpTransport, HttpTransport, Session, SocketTransport, Transport

__all__ = [
    "AsyncHttpTransport",
    "Attachment",
    "AuthError",
    "BadRequest",
    "BatchError",
    "ConfigMismatch",
    "Edge",
    "EdgeRelation",
    "Embedding",
    "Entity",
    "FrameTooLarge",
    "HttpError",
    "HttpTransport",
    "Incidence",
    "Khive",
    "KhiveError",
    "Note",
    "OpResult",
    "OperationError",
    "Page",
    "ProtocolMismatch",
    "RateLimited",
    "RequestRejected",
    "ServerError",
    "Session",
    "SocketTransport",
    "Transport",
    "TransportError",
    "cloud",
    "encode",
    "http_op_error_code",
    "op",
]

__version__ = "0.2.0"
