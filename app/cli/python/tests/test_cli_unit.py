"""CLI units: argument parsing (flags on either side of the subcommand),
key resolution, transport selection, the JWT cache keyed per (server,
wallet), and the 401 -> fresh-login retry. No server; the network is a fake
transport injected under `_make_transport`."""

from __future__ import annotations

import argparse
import asyncio
import json

import pytest

from pocketskynet_client import cli
from pocketskynet_client.transport import (
    Http3Transport,
    HttpTransport,
    Transport,
    TransportResponse,
)

ALICE = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
ALICE_KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"

_CHALLENGE = (
    "Welcome to FruitNation!\n\n"
    "Click to sign in and accept the FruitNation Terms of Service.\n\n"
    "This request will not trigger a blockchain transaction or cost any gas "
    "fees.\n\n"
    f"Wallet address:\n{ALICE}\n\nNonce:\n{'0' * 64}"
)


# ------------------------------------------------------------- parsing --


def _parse(argv):
    args = cli.build_parser().parse_args(argv)
    for name, default in cli._GLOBAL_DEFAULTS.items():
        if not hasattr(args, name):
            setattr(args, name, default)
    return args


def test_global_flags_accepted_before_the_subcommand():
    args = _parse(["--server", "https://x:1", "--http3", "--insecure", "rooms"])
    assert (args.server, args.http3, args.insecure) == ("https://x:1", True, True)


def test_global_flags_accepted_after_the_subcommand():
    args = _parse(["rooms", "--server", "https://x:1", "--http3"])
    assert (args.server, args.http3) == ("https://x:1", True)


def test_a_flag_before_the_subcommand_is_not_clobbered_by_defaults():
    # the SUPPRESS defaults exist exactly so the subparser cannot reset this
    args = _parse(["--key", "0xabc", "rooms"])
    assert args.key == "0xabc"


def test_unset_flags_get_their_defaults():
    args = _parse(["rooms"])
    assert args.http3 is False
    assert args.insecure is False
    assert args.key is None
    assert args.username is None


def test_positional_arguments_reach_the_namespace():
    args = _parse(["send", "room_abcdefgh12", "hello world"])
    assert (args.room_id, args.text) == ("room_abcdefgh12", "hello world")
    args = _parse(["messages", "room_abcdefgh12", "--limit", "7"])
    assert args.limit == 7


def test_a_subcommand_is_required():
    with pytest.raises(SystemExit):
        cli.build_parser().parse_args([])


# --------------------------------------------------- transport selection --


def _namespace(**overrides):
    values = {
        "server": "https://127.0.0.1:9099",
        "http3": False,
        "insecure": True,
        "key": None,
        "username": None,
    }
    values.update(overrides)
    return argparse.Namespace(**values)


def test_transport_selection_http1_by_default():
    transport = cli._make_transport(_namespace())
    assert isinstance(transport, HttpTransport)
    asyncio.run(transport.aclose())


def test_transport_selection_http3_with_the_flag():
    transport = cli._make_transport(_namespace(http3=True))
    assert isinstance(transport, Http3Transport)


def test_http3_with_a_plain_http_url_fails_loudly():
    with pytest.raises(ValueError, match="https"):
        cli._make_transport(_namespace(http3=True, server="http://127.0.0.1:9099"))


# --------------------------------------------------------- key resolution --


def test_key_flag_wins(monkeypatch):
    monkeypatch.setenv(cli.KEY_ENV_VAR, "0xenv")
    assert cli._require_key(_namespace(key="0xflag")) == "0xflag"


def test_key_env_var_is_the_fallback(monkeypatch):
    monkeypatch.setenv(cli.KEY_ENV_VAR, "0xenv")
    assert cli._require_key(_namespace()) == "0xenv"


def test_missing_key_exits_with_a_clear_error(monkeypatch):
    monkeypatch.delenv(cli.KEY_ENV_VAR, raising=False)
    with pytest.raises(SystemExit, match="POCKETSKYNET_KEY"):
        cli._require_key(_namespace())


# ------------------------------------------------------------- JWT cache --


@pytest.fixture
def session_file(tmp_path, monkeypatch):
    path = tmp_path / "session.json"
    monkeypatch.setattr(cli, "_session_file", lambda: path)
    return path


def test_sessions_are_keyed_per_server_and_wallet(session_file):
    cli._save_session("https://a:1", "0xaaa", "token-a")
    cli._save_session("https://b:2", "0xaaa", "token-b")
    cli._save_session("https://a:1", "0xbbb", "token-c")
    assert cli._cached_token("https://a:1", "0xaaa") == "token-a"
    assert cli._cached_token("https://b:2", "0xaaa") == "token-b"
    assert cli._cached_token("https://a:1", "0xbbb") == "token-c"
    assert cli._cached_token("https://c:3", "0xaaa") is None


def test_saving_overwrites_only_its_own_entry(session_file):
    cli._save_session("https://a:1", "0xaaa", "old")
    cli._save_session("https://a:1", "0xbbb", "other")
    cli._save_session("https://a:1", "0xaaa", "new")
    assert cli._cached_token("https://a:1", "0xaaa") == "new"
    assert cli._cached_token("https://a:1", "0xbbb") == "other"


def test_a_corrupt_session_file_reads_as_empty(session_file):
    session_file.write_text("{not json")
    assert cli._cached_token("https://a:1", "0xaaa") is None


def test_the_cache_key_normalizes_scheme_host_and_trailing_slash(session_file):
    cli._save_session("https://Host:9099/", "0xAAA", "tok")
    # trailing slash, uppercase host, uppercase address -> same entry
    assert cli._cached_token("https://host:9099", "0xaaa") == "tok"
    assert cli._cached_token("HTTPS://HOST:9099/", "0xAAA") == "tok"
    # a different port is still a different server
    assert cli._cached_token("https://host:9100", "0xaaa") is None
    # only one entry was written, not several near-duplicates
    assert len(cli._load_sessions()) == 1


def test_the_session_file_is_private(session_file):
    cli._save_session("https://a:1", "0xaaa", "secret")
    assert (session_file.stat().st_mode & 0o777) == 0o600


def test_the_session_file_is_never_briefly_world_readable(session_file, monkeypatch):
    """The write goes through a 0600 temp file + atomic rename, so a reader
    can never catch the token in a mode-0644 window."""
    seen_modes = []
    real_replace = cli.os.replace

    def spy_replace(src, dst):
        # at the moment of publish, the source (about to become dst) is 0600
        seen_modes.append(cli.os.stat(src).st_mode & 0o777)
        return real_replace(src, dst)

    monkeypatch.setattr(cli.os, "replace", spy_replace)
    cli._save_session("https://a:1", "0xaaa", "secret")
    assert seen_modes == [0o600]


# --------------------------------------------------- 401 -> fresh login --


class _RetryFakeTransport(Transport):
    """Fake server: rejects the stale cached token once, accepts the token
    minted by the re-login."""

    def __init__(self):
        self.paths: list[str] = []

    async def request(self, method, path, headers=None, body=None):
        self.paths.append(path)
        headers = {k.lower(): v for k, v in (headers or {}).items()}
        if path == "/api/auth/challenge":
            return _json(
                200,
                {
                    "challengeId": "cid",
                    "message": _CHALLENGE,
                    "expiresAt": "2099-01-01T00:00:00.000Z",
                },
            )
        if path == "/api/auth/login":
            return _json(
                200,
                {
                    "user": {"walletAddress": ALICE, "username": "alice"},
                    "token": "fresh",
                },
            )
        if path == "/api/rooms":
            if headers.get("authorization") == "Bearer fresh":
                return _json(200, [{"id": "room_ok12345678", "name": "ok"}])
            return _json(401, {"message": "Invalid token"})
        raise AssertionError(f"unexpected {path}")


def _json(status, payload):
    return TransportResponse(status, {}, json.dumps(payload).encode())


def test_a_stale_cached_token_triggers_one_relogin(session_file, monkeypatch):
    transport = _RetryFakeTransport()
    monkeypatch.setattr(cli, "_make_transport", lambda args: transport)
    args = _namespace(key=ALICE_KEY)
    cli._save_session(args.server, ALICE, "stale")

    rooms = asyncio.run(cli._run_authed(args, lambda client: client.rooms()))

    assert rooms == [{"id": "room_ok12345678", "name": "ok"}]
    # first attempt with the stale token, then challenge -> login -> retry
    assert transport.paths == [
        "/api/rooms",
        "/api/auth/challenge",
        "/api/auth/login",
        "/api/rooms",
    ]
    assert cli._cached_token(args.server, ALICE) == "fresh"


def test_a_cache_miss_logs_in_before_the_first_call(session_file, monkeypatch):
    transport = _RetryFakeTransport()
    monkeypatch.setattr(cli, "_make_transport", lambda args: transport)
    args = _namespace(key=ALICE_KEY)

    rooms = asyncio.run(cli._run_authed(args, lambda client: client.rooms()))

    assert rooms[0]["id"] == "room_ok12345678"
    assert transport.paths == ["/api/auth/challenge", "/api/auth/login", "/api/rooms"]
    assert cli._cached_token(args.server, ALICE) == "fresh"


def test_a_valid_cached_token_is_used_without_logging_in(session_file, monkeypatch):
    transport = _RetryFakeTransport()
    monkeypatch.setattr(cli, "_make_transport", lambda args: transport)
    args = _namespace(key=ALICE_KEY)
    cli._save_session(args.server, ALICE, "fresh")

    asyncio.run(cli._run_authed(args, lambda client: client.rooms()))

    assert transport.paths == ["/api/rooms"]
