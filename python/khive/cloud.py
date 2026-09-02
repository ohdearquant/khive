"""`khive.cloud(base_url, api_key)` — the one-line entry point for khive-cloud.

`Khive` itself takes any `Transport` (`Khive(transport=HttpTransport(url,
key))` works today); this is sugar for that, kept out of `client.py` because
the facade there is a different author's design and stays untouched.
"""

from __future__ import annotations

from typing import Any

from .client import Khive
from .transport import HttpTransport


def cloud(base_url: str, api_key: str, **kwargs: Any) -> Khive:
    """Build a `Khive` client backed by a khive-cloud deployment.

    >>> db = khive.cloud("https://khive-cloud.example", api_key)
    >>> db.stats()
    """
    return Khive(transport=HttpTransport(base_url, api_key), **kwargs)
