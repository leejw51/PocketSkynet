"""Errors raised by the PocketSkynet client."""

from __future__ import annotations

__all__ = ["ApiError", "TransportError"]


class TransportError(Exception):
    """The request never produced an HTTP response (connect/TLS/QUIC failure)."""


class ApiError(Exception):
    """A non-2xx HTTP response from the server.

    ``message`` mirrors the server's error envelope (section 1.5 of API.md);
    ``code`` carries the machine-readable code the two 409 message-post
    failures emit (KEY_ROTATION_REQUIRED / STALE_KEY_VERSION).
    """

    def __init__(self, status: int, message: str, code: str | None = None):
        super().__init__(f"HTTP {status}: {message}" + (f" [{code}]" if code else ""))
        self.status = status
        self.message = message
        self.code = code
