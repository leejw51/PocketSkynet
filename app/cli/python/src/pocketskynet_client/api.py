"""Transport-agnostic API client for the PocketSkynet server.

Request bodies are camelCase JSON (API.md section 1.4). Authentication is a
JWT sent as ``Authorization: Bearer <token>``, obtained by the
challenge -> personal_sign -> login flow (PROTOCOL.md section 4):

1. ``POST /api/auth/challenge {walletAddress}`` returns a challenge string.
2. Sign the challenge string *verbatim* with EIP-191 personal_sign.
3. ``POST /api/auth/login {walletAddress, challengeId, signature, ...}``
   returns the JWT.
"""

from __future__ import annotations

import hashlib
import json
from typing import Any, Self

from .crypto import parse_private_key, personal_sign, private_key_to_address
from .errors import ApiError
from .transport import Transport, TransportResponse

__all__ = [
    "Client",
    "LoginResult",
    "challenge_request",
    "create_room_request",
    "login_request",
    "message_hash",
    "send_message_request",
]


def challenge_request(wallet_address: str) -> dict[str, Any]:
    """Body of ``POST /api/auth/challenge``."""
    return {"walletAddress": wallet_address}


def login_request(
    wallet_address: str,
    challenge_id: str,
    signature: str,
    username: str | None = None,
) -> dict[str, Any]:
    """Body of ``POST /api/auth/login``.

    ``username`` is omitted when not given: an empty username makes the
    server reuse the stored one, and it is only required on first login.
    """
    body: dict[str, Any] = {
        "walletAddress": wallet_address,
        "challengeId": challenge_id,
        "signature": signature,
    }
    if username:
        body["username"] = username
    return body


def create_room_request(name: str, description: str | None = None) -> dict[str, Any]:
    """Body of ``POST /api/rooms``."""
    body: dict[str, Any] = {"name": name}
    if description is not None:
        body["description"] = description
    return body


def message_hash(content: str) -> str:
    """msgHash for a plaintext message: lowercase hex SHA-256 of the trimmed
    content (the server trims before storing -- PROTOCOL.md section 13)."""
    return hashlib.sha256(content.strip().encode("utf-8")).hexdigest()


def send_message_request(content: str) -> dict[str, Any]:
    """Body of ``POST /api/rooms/{roomId}/messages`` for a plaintext message."""
    trimmed = content.strip()
    return {"content": trimmed, "msgHash": message_hash(trimmed)}


class LoginResult:
    """What ``POST /api/auth/login`` returned."""

    def __init__(self, raw: dict[str, Any]):
        self.raw = raw
        self.token: str = raw["token"]
        self.user: dict[str, Any] = raw.get("user", {})
        self.wallet_address: str = self.user.get("walletAddress", "")
        self.encryption_salt: str | None = raw.get("encryptionSalt")


class Client:
    """High-level API client. Give it any `Transport`; it never cares which."""

    def __init__(self, transport: Transport, token: str | None = None):
        self._transport = transport
        self.token = token

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self._transport.aclose()

    # ----------------------------------------------------------- plumbing --

    async def _call(
        self,
        method: str,
        path: str,
        body: dict[str, Any] | None = None,
        authenticated: bool = True,
    ) -> Any:
        headers: dict[str, str] = {"accept": "application/json"}
        payload: bytes | None = None
        if body is not None:
            headers["content-type"] = "application/json"
            payload = json.dumps(body, ensure_ascii=False).encode("utf-8")
        if authenticated and self.token:
            headers["authorization"] = f"Bearer {self.token}"
        response = await self._transport.request(
            method, path, headers=headers, body=payload
        )
        return self._parse(response)

    @staticmethod
    def _parse(response: TransportResponse) -> Any:
        try:
            decoded = json.loads(response.body) if response.body else None
        except ValueError:
            decoded = None
        if 200 <= response.status < 300:
            return decoded
        message = "Request failed"
        code = None
        if isinstance(decoded, dict):
            message = decoded.get("message", message)
            code = decoded.get("code")
            if decoded.get("errors"):
                message = f"{message}: {'; '.join(decoded['errors'])}"
        raise ApiError(response.status, message, code)

    # --------------------------------------------------------------- auth --

    async def challenge(self, wallet_address: str) -> dict[str, Any]:
        return await self._call(
            "POST",
            "/api/auth/challenge",
            challenge_request(wallet_address),
            authenticated=False,
        )

    async def login(
        self,
        wallet_address: str,
        challenge_id: str,
        signature: str,
        username: str | None = None,
    ) -> LoginResult:
        raw = await self._call(
            "POST",
            "/api/auth/login",
            login_request(wallet_address, challenge_id, signature, username),
            authenticated=False,
        )
        result = LoginResult(raw)
        self.token = result.token
        return result

    async def login_with_key(
        self, private_key: str, username: str | None = None
    ) -> LoginResult:
        """Full auth flow: challenge -> sign verbatim -> login -> JWT.

        On a first-time login with no username given, retries once with a
        deterministic default (a failed login burns the challenge, so the
        retry requests a fresh one).
        """
        key = parse_private_key(private_key)
        address = private_key_to_address(key)

        async def attempt(name: str | None) -> LoginResult:
            challenge = await self.challenge(address)
            signature = personal_sign(key, challenge["message"])
            return await self.login(address, challenge["challengeId"], signature, name)

        try:
            return await attempt(username)
        except ApiError as exc:
            if (
                username is None
                and exc.status == 400
                and "Username is required" in exc.message
            ):
                return await attempt(f"user_{address[2:10]}")
            raise

    # ---------------------------------------------------------------- api --

    async def health(self) -> dict[str, Any]:
        return await self._call("GET", "/api/health", authenticated=False)

    async def rooms(self) -> list[dict[str, Any]]:
        return await self._call("GET", "/api/rooms")

    async def create_room(
        self, name: str, description: str | None = None
    ) -> dict[str, Any]:
        return await self._call(
            "POST", "/api/rooms", create_room_request(name, description)
        )

    async def send_message(self, room_id: str, content: str) -> dict[str, Any]:
        return await self._call(
            "POST",
            f"/api/rooms/{room_id}/messages",
            send_message_request(content),
        )

    async def messages(
        self,
        room_id: str,
        limit: int = 50,
        before: int | None = None,
        since: int | None = None,
    ) -> list[dict[str, Any]]:
        query = f"limit={limit}"
        if before is not None:
            query += f"&before={before}"
        if since is not None:
            query += f"&since={since}"
        return await self._call("GET", f"/api/rooms/{room_id}/messages?{query}")
