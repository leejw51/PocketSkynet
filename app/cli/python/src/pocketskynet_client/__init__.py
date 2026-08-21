"""Python client for the PocketSkynet messenger server.

Library entry points:

- `pocketskynet_client.crypto` -- keccak256, key -> address, EIP-191 signing.
- `pocketskynet_client.transport` -- HTTP/1.1 (httpx) and HTTP/3 (aioquic)
  behind one `Transport` interface.
- `pocketskynet_client.api.Client` -- the transport-agnostic API client.

CLI: the `pskynet` console script (`pocketskynet_client.cli`).
"""

from .api import Client, LoginResult
from .errors import ApiError, TransportError
from .transport import Http3Transport, HttpTransport, Transport

__all__ = [
    "ApiError",
    "Client",
    "Http3Transport",
    "HttpTransport",
    "LoginResult",
    "Transport",
    "TransportError",
]

__version__ = "0.1.0"
