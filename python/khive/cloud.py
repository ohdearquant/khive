"""Build a khive-cloud client from explicit arguments or environment configuration."""

from __future__ import annotations

import os
from typing import Any

from .client import Khive
from .transport import HttpTransport


def _cloud_configuration(base_url: str | None, api_key: str | None) -> tuple[str, str]:
    base_url = os.environ.get("KHIVE_CLOUD_URL") if base_url is None else base_url
    api_key = os.environ.get("KHIVE_CLOUD_API_KEY") if api_key is None else api_key
    if not base_url:
        raise ValueError("base_url or KHIVE_CLOUD_URL is required; there is no default URL")
    if not api_key:
        raise ValueError("api_key or KHIVE_CLOUD_API_KEY is required")
    return base_url, api_key


def cloud(
    base_url: str | None = None,
    api_key: str | None = None,
    *,
    allow_insecure: bool = False,
    **kwargs: Any,
) -> Khive:
    """Build a `Khive` client backed by a khive-cloud deployment.

    >>> db = khive.cloud("https://khive-cloud.example", api_key)
    >>> db.stats()

    Explicit arguments override `KHIVE_CLOUD_URL` and `KHIVE_CLOUD_API_KEY`.
    Neither value has a default; missing configuration raises `ValueError`.

    `allow_insecure=True` permits a plain `http://` base URL whose host is
    not loopback — off by default, since that sends the API key over an
    unencrypted connection (see `HttpTransport`).
    """
    base_url, api_key = _cloud_configuration(base_url, api_key)
    return Khive(
        transport=HttpTransport(base_url, api_key, allow_insecure=allow_insecure), **kwargs
    )
