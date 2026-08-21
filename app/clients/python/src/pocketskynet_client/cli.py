"""Command-line interface: the `pskynet` console script.

Global flags choose the server and transport; subcommands map 1:1 onto API
calls. The private key comes from `--key` or the POCKETSKYNET_KEY
environment variable. After a login the JWT is cached per (server, wallet)
under ~/.config/pocketskynet-client/ so subsequent commands do not burn the
5/min login rate limit; an expired token transparently re-runs the flow.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
from pathlib import Path
from typing import Any

from .api import Client
from .crypto import parse_private_key, private_key_to_address
from .errors import ApiError, TransportError
from .transport import Http3Transport, HttpTransport, Transport

DEFAULT_SERVER = "https://127.0.0.1:9099"
KEY_ENV_VAR = "POCKETSKYNET_KEY"


def _session_file() -> Path:
    base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(base) / "pocketskynet-client" / "session.json"


def _load_sessions() -> dict[str, Any]:
    try:
        return json.loads(_session_file().read_text())
    except (OSError, ValueError):
        return {}


def _save_session(server: str, address: str, token: str) -> None:
    sessions = _load_sessions()
    sessions[f"{server}|{address}"] = {"token": token}
    path = _session_file()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(sessions, indent=2))
    path.chmod(0o600)


def _cached_token(server: str, address: str) -> str | None:
    entry = _load_sessions().get(f"{server}|{address}")
    return entry.get("token") if isinstance(entry, dict) else None


def _make_transport(args: argparse.Namespace) -> Transport:
    if args.http3:
        return Http3Transport(args.server, insecure=args.insecure)
    return HttpTransport(args.server, insecure=args.insecure)


def _require_key(args: argparse.Namespace) -> str:
    key = args.key or os.environ.get(KEY_ENV_VAR)
    if not key:
        raise SystemExit(f"error: no private key -- pass --key or set {KEY_ENV_VAR}")
    return key


def _print(data: Any) -> None:
    print(json.dumps(data, indent=2, ensure_ascii=False))


async def _run_authed(args: argparse.Namespace, call) -> Any:
    """Run one authenticated call, logging in only when the token cache
    misses and retrying once through a fresh login on 401 (expired JWT)."""
    key = _require_key(args)
    address = private_key_to_address(parse_private_key(key))
    async with Client(
        _make_transport(args), token=_cached_token(args.server, address)
    ) as client:
        if client.token is None:
            result = await client.login_with_key(key, args.username)
            _save_session(args.server, address, result.token)
        try:
            return await call(client)
        except ApiError as exc:
            if exc.status != 401:
                raise
            result = await client.login_with_key(key, args.username)
            _save_session(args.server, address, result.token)
            return await call(client)


async def cmd_health(args: argparse.Namespace) -> None:
    async with Client(_make_transport(args)) as client:
        _print(await client.health())


async def cmd_login(args: argparse.Namespace) -> None:
    key = _require_key(args)
    async with Client(_make_transport(args)) as client:
        result = await client.login_with_key(key, args.username)
        _save_session(args.server, result.wallet_address, result.token)
        _print(
            {
                "walletAddress": result.wallet_address,
                "username": result.user.get("username"),
                "token": result.token,
            }
        )


async def cmd_rooms(args: argparse.Namespace) -> None:
    rooms = await _run_authed(args, lambda c: c.rooms())
    if args.json:
        _print(rooms)
        return
    if not rooms:
        print("(no rooms)")
    for room in rooms:
        line = f"{room['id']}  {room.get('name') or '(unnamed)'}"
        unread = room.get("unreadCount")
        if unread:
            line += f"  [{unread} unread]"
        print(line)


async def cmd_create_room(args: argparse.Namespace) -> None:
    room = await _run_authed(args, lambda c: c.create_room(args.name, args.description))
    _print(room)


async def cmd_send(args: argparse.Namespace) -> None:
    message = await _run_authed(args, lambda c: c.send_message(args.room_id, args.text))
    _print(message)


async def cmd_messages(args: argparse.Namespace) -> None:
    messages = await _run_authed(
        args, lambda c: c.messages(args.room_id, limit=args.limit)
    )
    if args.json:
        _print(messages)
        return
    if not messages:
        print("(no messages)")
    for message in messages:
        sender = message.get("sender") or {}
        who = sender.get("username") or message.get("senderAddress", "?")
        body = (
            "<encrypted>" if message.get("isEncrypted") else message.get("content", "")
        )
        print(f"[{message.get('createdAt', '')}] {who}: {body}")


_GLOBAL_DEFAULTS = {
    "server": None,  # resolved in main(): env POCKETSKYNET_SERVER, else DEFAULT_SERVER
    "http3": False,
    "insecure": False,
    "key": None,
    "username": None,
}


def build_parser() -> argparse.ArgumentParser:
    # The global flags live in a parent parser attached to the main parser
    # *and* every subparser, so they are accepted both before and after the
    # subcommand. SUPPRESS keeps a subparser from clobbering a value given
    # before the subcommand; unset flags get their defaults in main().
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument(
        "--server",
        default=argparse.SUPPRESS,
        help=f"server base URL (default {DEFAULT_SERVER}, env POCKETSKYNET_SERVER)",
    )
    common.add_argument(
        "--http3",
        action="store_true",
        default=argparse.SUPPRESS,
        help="use HTTP/3 over QUIC (requires an https:// server URL)",
    )
    common.add_argument(
        "--insecure",
        action="store_true",
        default=argparse.SUPPRESS,
        help="skip TLS certificate verification (self-signed dev certs)",
    )
    common.add_argument(
        "--key",
        default=argparse.SUPPRESS,
        help=f"wallet private key hex (or set {KEY_ENV_VAR})",
    )
    common.add_argument(
        "--username",
        default=argparse.SUPPRESS,
        help="username for first-time login (otherwise auto-generated)",
    )

    parser = argparse.ArgumentParser(
        prog="pskynet",
        parents=[common],
        description="PocketSkynet client -- wallet login, rooms, and messages "
        "over HTTP/1.1 or HTTP/3.",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("health", parents=[common], help="GET /api/health")
    p.set_defaults(func=cmd_health)

    p = sub.add_parser("login", parents=[common], help="challenge -> sign -> JWT")
    p.set_defaults(func=cmd_login)

    p = sub.add_parser("rooms", parents=[common], help="list your rooms")
    p.add_argument("--json", action="store_true", help="print the raw JSON")
    p.set_defaults(func=cmd_rooms)

    p = sub.add_parser("create-room", parents=[common], help="create a room")
    p.add_argument("name")
    p.add_argument("--description")
    p.set_defaults(func=cmd_create_room)

    p = sub.add_parser("send", parents=[common], help="send a plaintext message")
    p.add_argument("room_id")
    p.add_argument("text")
    p.set_defaults(func=cmd_send)

    p = sub.add_parser("messages", parents=[common], help="list a room's messages")
    p.add_argument("room_id")
    p.add_argument("--limit", type=int, default=50)
    p.add_argument("--json", action="store_true", help="print the raw JSON")
    p.set_defaults(func=cmd_messages)

    return parser


def main(argv: list[str] | None = None) -> None:
    args = build_parser().parse_args(argv)
    for name, default in _GLOBAL_DEFAULTS.items():
        if not hasattr(args, name):
            setattr(args, name, default)
    if args.server is None:
        args.server = os.environ.get("POCKETSKYNET_SERVER", DEFAULT_SERVER)
    try:
        asyncio.run(args.func(args))
    except ApiError as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(1) from exc
    except TransportError as exc:
        print(f"transport error: {exc}", file=sys.stderr)
        raise SystemExit(2) from exc
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(1) from exc


if __name__ == "__main__":
    main()
