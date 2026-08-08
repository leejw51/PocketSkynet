"""End-to-end flows against a live server, excluding blockchain features.

Each flow is a function taking the shared context; `run.py` runs them in
order and reports per-flow pass/fail. Flows share the logged-in users but
create their own rooms, so a failure in one leaves the others meaningful.

Blockchain-dependent surface (shout, sites, message publish — anything
needing VITE_FRUITNATION_WALLET and a payment) is deliberately not here.
"""

import hashlib
import http.client
import secrets
import socket
import time
import urllib.error
import urllib.parse

from client import Api
from ethwallet import Wallet, keccak256


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def msg_hash(content: str) -> str:
    return sha256_hex(content.encode())


def check(cond, why):
    if not cond:
        raise AssertionError(why)


class User:
    def __init__(self, wallet: Wallet, api: Api, login_body: dict):
        self.wallet = wallet
        self.api = api
        self.address = login_body["user"]["walletAddress"]
        self.username = login_body["user"]["username"]
        self.encryption_salt = login_body["encryptionSalt"]
        self.is_server_admin = login_body["isServerAdmin"]


def login(
    base_url: str, wallet: Wallet, username: str | None, ca_file: str | None = None
) -> User:
    anon = Api(base_url, ca_file=ca_file)
    challenge = (
        anon.post("/api/auth/challenge", {"walletAddress": wallet.address})
        .expect(200, "challenge")
        .json()
    )
    body = {
        "walletAddress": wallet.address,
        "challengeId": challenge["challengeId"],
        "signature": wallet.personal_sign(challenge["message"]),
    }
    if username is not None:
        body["username"] = username
    resp = anon.post("/api/auth/login", body).expect(200, "login").json()
    return User(wallet, Api(base_url, resp["token"], ca_file=ca_file), resp)


class Context:
    """Shared state: the backend URL and the cast of logged-in users."""

    def __init__(self, base_url, admin_wallet: Wallet):
        self.base_url = base_url
        self.admin_wallet = admin_wallet
        self.admin = None
        self.alice = None
        self.bob = None
        self.carol = None
        self.dave = None
        self.room_id = None

    def setup(self):
        """Log the cast in and build the shared room every flow can lean on.

        Runs before any flow, so a filtered run (`run.py messages`) starts
        from the same world as a full one. The shared room keeps a fixed
        shape — alice its only admin, bob a plain member — and flows that
        mutate roles or membership make their own rooms.
        """
        self.alice = login(self.base_url, Wallet(), "alice")
        self.bob = login(self.base_url, Wallet(), "bob")
        self.carol = login(self.base_url, Wallet(), "carol")
        self.dave = login(self.base_url, Wallet(), "dave")
        self.admin = login(self.base_url, self.admin_wallet, "the_admin")
        self.room_id = self.new_room(self.alice, "Shared HQ")
        self.add_member(self.room_id, self.alice, self.bob)

    def new_room(self, owner: User, name: str) -> str:
        room = (
            owner.api.post("/api/rooms", {"name": name})
            .expect(200, "create room")
            .json()
        )
        return room["id"]

    def add_member(self, room_id: str, admin: User, invitee: User):
        admin.api.post(
            f"/api/rooms/{room_id}/invite", {"userAddress": invitee.address}
        ).expect(200, "invite")
        invitee.api.post(f"/api/invitations/{room_id}/accept").expect(200, "accept")


# --- flows ------------------------------------------------------------------


def flow_health_and_info(ctx):
    anon = Api(ctx.base_url)
    health = anon.get("/api/health").expect(200).json()
    check(health["status"] == "ok", f"health status {health}")
    info = anon.get("/api/server/info").expect(200).json()
    check(info["scheme"] == "http", f"scheme {info}")
    networks = anon.get("/api/networks").expect(200).json()
    check(
        isinstance(networks, list) and networks, "networks should be a non-empty list"
    )
    chain = anon.get("/api/blockchain/info").expect(200).json()
    check("chainId" in chain, f"blockchain info {chain}")
    anon.get("/api/nope").expect(404, "unknown route is a JSON 404")


def flow_auth(ctx):
    check(
        ctx.alice.address == ctx.alice.wallet.address.lower(), "address is lowercased"
    )
    check(len(ctx.alice.encryption_salt) == 64, "encryptionSalt is 64 hex")
    check(not ctx.alice.is_server_admin, "alice is not a server admin")
    check(ctx.admin.is_server_admin, "VITE_FRUITNATION_ADMIN grants isServerAdmin")

    anon = Api(ctx.base_url)
    # A signature from the wrong key is a 401, and the challenge burns.
    challenge = (
        anon.post("/api/auth/challenge", {"walletAddress": ctx.alice.wallet.address})
        .expect(200)
        .json()
    )
    intruder = Wallet()
    anon.post(
        "/api/auth/login",
        {
            "walletAddress": ctx.alice.wallet.address,
            "challengeId": challenge["challengeId"],
            "signature": intruder.personal_sign(challenge["message"]),
        },
    ).expect(401, "wrong signer")
    anon.post(
        "/api/auth/login",
        {
            "walletAddress": ctx.alice.wallet.address,
            "challengeId": challenge["challengeId"],
            "signature": ctx.alice.wallet.personal_sign(challenge["message"]),
        },
    ).expect(400, "a challenge is single-use, even after a failed attempt")

    # Second login without a username reuses the stored one.
    again = login(ctx.base_url, ctx.alice.wallet, None)
    check(again.username == "alice", f"username reused, got {again.username}")

    anon.get("/api/auth/profile").expect(401, "no token")
    Api(ctx.base_url, "garbage").get("/api/auth/profile").expect(401, "bad token")
    anon.post("/api/auth/challenge", {"walletAddress": "not-an-address"}).expect(400)


def flow_profile_and_users(ctx):
    alice, bob = ctx.alice, ctx.bob
    profile = alice.api.get("/api/auth/profile").expect(200).json()
    check(profile["username"] == "alice", f"profile {profile}")

    alice.api.put("/api/auth/profile", {"username": "ab"}).expect(400, "too short")
    alice.api.put("/api/auth/profile", {"username": "alice<script>"}).expect(
        400, "bad chars"
    )
    updated = (
        alice.api.put("/api/auth/profile", {"username": "alice_체셔"})
        .expect(200)
        .json()
    )
    check(updated["username"] == "alice_체셔", "unicode username accepted")
    alice.api.put("/api/auth/profile", {"username": "alice"}).expect(200)

    other = alice.api.get(f"/api/users/{bob.wallet.address}").expect(200).json()
    check(
        other["walletAddress"] == bob.address, "mixed-case lookup, lowercase response"
    )
    alice.api.get("/api/users/0x0000000000000000000000000000000000000001").expect(404)

    alice.api.get("/api/users/search").expect(400, "q is required")
    hits = alice.api.get("/api/users/search?q=bob").expect(200).json()
    check(any(u["walletAddress"] == bob.address for u in hits), "search finds bob")


def flow_rooms(ctx):
    alice, bob = ctx.alice, ctx.bob
    room = (
        alice.api.post("/api/rooms", {"name": "HQ", "description": "hi"})
        .expect(200)
        .json()
    )
    room_id = room["id"]
    check(room["kind"] == "channel", f"kind {room}")

    alice.api.post("/api/rooms", {"name": "   "}).expect(400, "whitespace name")
    listed = alice.api.get("/api/rooms").expect(200).json()
    mine = next(r for r in listed if r["id"] == room_id)
    check(
        mine["unreadCount"] == 0 and "mentionCount" in mine, f"room list shape {mine}"
    )

    got = alice.api.get(f"/api/rooms/{room_id}").expect(200).json()
    check(
        got["memberCount"] == 1 and len(got["admins"]) == 1,
        f"creator is the member+admin {got}",
    )

    bob.api.get(f"/api/rooms/{room_id}").expect(403, "non-member")
    alice.api.get("/api/rooms/room_0000000000_nope").expect(
        403, "nonexistent room is 403, no oracle"
    )
    alice.api.get("/api/rooms/x").expect(400, "malformed roomId")

    bob.api.patch(f"/api/rooms/{room_id}", {"name": "Bob HQ"}).expect(403)
    renamed = (
        alice.api.patch(f"/api/rooms/{room_id}", {"name": "HQ2"}).expect(200).json()
    )
    check(renamed["name"] == "HQ2", "rename applied")


def flow_invitations(ctx):
    alice, bob, carol = ctx.alice, ctx.bob, ctx.carol
    room_id = ctx.new_room(alice, "Invites")
    bob.api.post(f"/api/rooms/{room_id}/invite", {"userAddress": carol.address}).expect(
        403, "only admins invite"
    )
    alice.api.post(f"/api/rooms/{room_id}/invite", {"userAddress": bob.address}).expect(
        200
    )
    pending = bob.api.get("/api/invitations").expect(200).json()
    check(
        any(i["roomId"] == room_id for i in pending),
        f"bob sees the invitation {pending}",
    )
    bob.api.post(f"/api/invitations/{room_id}/accept").expect(200)
    bob.api.get(f"/api/rooms/{room_id}").expect(200, "member after accepting")
    alice.api.post(f"/api/rooms/{room_id}/invite", {"userAddress": bob.address}).expect(
        400, "already a member"
    )

    alice.api.post(
        f"/api/rooms/{room_id}/invite", {"userAddress": carol.address}
    ).expect(200)
    carol.api.post(f"/api/invitations/{room_id}/decline").expect(200)
    carol.api.get(f"/api/rooms/{room_id}").expect(403, "declining does not join")
    carol.api.post(f"/api/invitations/{room_id}/accept").expect(
        404, "invitation is gone"
    )

    # An address nobody holds an account for is a 404 — the invitee does not
    # exist. A high address so it does not collide with the reserved
    # `0x00000000…` prefix exercised just below.
    alice.api.post(
        f"/api/rooms/{room_id}/invite",
        {"userAddress": "0xdead00000000000000000000000000000000beef"},
    ).expect(404)

    # A reserved address — a webhook or agent sender, never a person — is
    # refused before the room is even consulted, so it is a 400 whether or not
    # any account claims it.
    alice.api.post(
        f"/api/rooms/{room_id}/invite",
        {"userAddress": "0x0000000000000000000000000000000000000002"},
    ).expect(400)


def flow_admins_and_kick(ctx):
    # Role churn happens in a room of its own — the shared room's shape
    # (alice the only admin, bob a plain member) is load-bearing elsewhere.
    alice, bob, carol = ctx.alice, ctx.bob, ctx.carol
    room_id = ctx.new_room(alice, "Boardroom")
    ctx.add_member(room_id, alice, bob)
    alice.api.post(
        f"/api/rooms/{room_id}/admins", {"walletAddress": carol.address}
    ).expect(400, "must be a member first")
    alice.api.post(
        f"/api/rooms/{room_id}/admins", {"walletAddress": bob.address}
    ).expect(200)
    admins = alice.api.get(f"/api/rooms/{room_id}/admins").expect(200).json()
    check(len(admins) == 2, f"two admins {admins}")
    alice.api.post(
        f"/api/rooms/{room_id}/admins", {"walletAddress": bob.address}
    ).expect(400, "already an admin")

    bob.api.delete(f"/api/rooms/{room_id}/admins/{alice.address}").expect(
        200, "demote alice"
    )
    bob.api.delete(f"/api/rooms/{room_id}/admins/{bob.address}").expect(
        400, "last admin stays"
    )
    bob.api.post(
        f"/api/rooms/{room_id}/admins", {"walletAddress": alice.address}
    ).expect(200)

    ctx.add_member(room_id, alice, carol)
    members = alice.api.get(f"/api/rooms/{room_id}/members").expect(200).json()
    check(len(members) == 3, f"three members {members}")
    alice.api.post(f"/api/rooms/{room_id}/kick", {"userAddress": alice.address}).expect(
        400, "cannot kick yourself"
    )
    kicked = (
        alice.api.post(f"/api/rooms/{room_id}/kick", {"userAddress": carol.address})
        .expect(200)
        .json()
    )
    check(kicked["keyRotationPending"] is True, f"kick flags rotation {kicked}")
    carol.api.get(f"/api/rooms/{room_id}").expect(403, "kicked out")


def flow_leave(ctx):
    alice, bob = ctx.alice, ctx.bob
    room_id = ctx.new_room(alice, "Leavers")
    ctx.add_member(room_id, alice, bob)
    alice.api.post(f"/api/rooms/{room_id}/leave").expect(400, "last admin cannot leave")
    alice.api.post(
        f"/api/rooms/{room_id}/admins", {"walletAddress": bob.address}
    ).expect(200)
    alice.api.post(f"/api/rooms/{room_id}/leave").expect(200)
    alice.api.get(f"/api/rooms/{room_id}").expect(403, "gone after leaving")
    bob.api.post(f"/api/rooms/{room_id}/leave").expect(400, "bob is now the last admin")
    bob.api.delete(f"/api/rooms/{room_id}").expect(
        200, "so he deletes the room instead"
    )


def flow_dm(ctx):
    alice, bob, carol = ctx.alice, ctx.bob, ctx.carol
    dm = (
        alice.api.post("/api/rooms/dm", {"walletAddress": bob.address})
        .expect(200)
        .json()
    )
    check(dm["kind"] == "dm" and dm["memberCount"] == 2, f"dm shape {dm}")
    again = (
        bob.api.post("/api/rooms/dm", {"walletAddress": alice.address})
        .expect(200)
        .json()
    )
    check(again["id"] == dm["id"], "DM is idempotent by member set")

    group = (
        alice.api.post(
            "/api/rooms/dm", {"walletAddresses": [bob.address, carol.address]}
        )
        .expect(200)
        .json()
    )
    check(
        group["kind"] == "group_dm" and group["memberCount"] == 3, f"group dm {group}"
    )

    alice.api.patch(f"/api/rooms/{dm['id']}", {"name": "renamed"}).expect(
        400, "DMs refuse rename"
    )
    alice.api.post(f"/api/rooms/{dm['id']}/leave").expect(400, "DMs refuse leave")
    unknown = Wallet()
    alice.api.post("/api/rooms/dm", {"walletAddress": unknown.address}).expect(
        404, "DM partner must have logged in before"
    )
    ctx.dm_id = dm["id"]


def flow_messages(ctx):
    alice, bob = ctx.alice, ctx.bob
    room_id = ctx.room_id
    content = "Hello everyone!"
    sent = (
        alice.api.post(
            f"/api/rooms/{room_id}/messages",
            {
                "content": content,
                "msgHash": msg_hash(content),
            },
        )
        .expect(200)
        .json()
    )
    check(sent["senderAddress"] == alice.address, "sender comes from the JWT")
    check(sent["msgType"] == "add" and sent["isDeleted"] is False, f"shape {sent}")
    mid = sent["id"]

    alice.api.post(
        f"/api/rooms/{room_id}/messages", {"content": "   ", "msgHash": "a" * 64}
    ).expect(400, "whitespace only")
    alice.api.post(
        f"/api/rooms/{room_id}/messages", {"content": "x" * 5001, "msgHash": "a" * 64}
    ).expect(400, "over 5000 chars")
    alice.api.post(
        f"/api/rooms/{room_id}/messages", {"content": "hi", "msgHash": "A" * 64}
    ).expect(400, "msgHash must be lowercase")
    ctx.carol.api.post(
        f"/api/rooms/{room_id}/messages", {"content": "hi", "msgHash": "a" * 64}
    ).expect(403, "non-member")

    listed = alice.api.get(f"/api/rooms/{room_id}/messages").expect(200).json()
    check(any(m["id"] == mid for m in listed), "message listed")

    bob.api.patch(
        f"/api/messages/{mid}", {"content": "hijack", "msgHash": "a" * 64}
    ).expect(403, "only the owner edits")
    edited = (
        alice.api.patch(
            f"/api/messages/{mid}",
            {
                "content": "Hello, edited!",
                "msgHash": msg_hash("Hello, edited!"),
            },
        )
        .expect(200)
        .json()
    )
    check(edited["msgType"] == "edit" and edited["editedAt"], f"edit shape {edited}")

    # Threads: replies hang off a parent and are folded out of the main list.
    reply = (
        bob.api.post(
            f"/api/rooms/{room_id}/messages",
            {
                "content": "a reply",
                "msgHash": msg_hash("a reply"),
                "parentMessageId": mid,
            },
        )
        .expect(200)
        .json()
    )
    check(reply["parentMessageId"] == mid, "reply links to parent")
    main_list = alice.api.get(f"/api/rooms/{room_id}/messages").expect(200).json()
    parent = next(m for m in main_list if m["id"] == mid)
    check(parent["replyCount"] == 1, f"replyCount {parent}")
    check(
        all(m["id"] != reply["id"] for m in main_list),
        "replies excluded from the main list",
    )
    thread = alice.api.get(f"/api/messages/{reply['id']}/thread").expect(200).json()
    check(
        [thread[0]["id"], thread[1]["id"]] == [mid, reply["id"]], "thread is root-first"
    )

    # Any member may delete; a second delete is a 404; deletion is soft.
    victim = (
        alice.api.post(
            f"/api/rooms/{room_id}/messages",
            {
                "content": "delete me",
                "msgHash": msg_hash("delete me"),
            },
        )
        .expect(200)
        .json()
    )
    bob.api.delete(f"/api/messages/{victim['id']}").expect(200, "any member can delete")
    bob.api.delete(f"/api/messages/{victim['id']}").expect(404, "second delete")

    bob.api.delete(f"/api/rooms/{room_id}/messages").expect(
        403, "a plain member cannot purge the history"
    )


def flow_emoticons(ctx):
    alice, bob = ctx.alice, ctx.bob
    room_id = ctx.room_id
    target = (
        alice.api.post(
            f"/api/rooms/{room_id}/messages",
            {
                "content": "react to me",
                "msgHash": msg_hash("react to me"),
            },
        )
        .expect(200)
        .json()
    )
    mid = target["id"]

    alice.api.post(f"/api/messages/{mid}/emoticons", {"emoticonCode": "🍎"}).expect(200)
    bob.api.post(f"/api/messages/{mid}/emoticons", {"emoticonCode": "🍎"}).expect(200)
    bob.api.post(f"/api/messages/{mid}/emoticons", {"emoticonCode": ""}).expect(400)

    agg = alice.api.get(f"/api/messages/{mid}/emoticons").expect(200).json()
    apple = next(e for e in agg if e["emoticonCode"] == "🍎")
    check(apple["count"] == 2 and len(apple["users"]) == 2, f"aggregation {agg}")

    code = urllib.parse.quote("🍎", safe="")  # percent-encoded exactly once
    bob.api.delete(f"/api/messages/{mid}/emoticons/{code}").expect(200)
    bob.api.delete(f"/api/messages/{mid}/emoticons/{code}").expect(
        200, "removal is idempotent"
    )
    agg = alice.api.get(f"/api/messages/{mid}/emoticons").expect(200).json()
    apple = next(e for e in agg if e["emoticonCode"] == "🍎")
    check(apple["count"] == 1, f"one reaction left {agg}")


def flow_mentions_and_read(ctx):
    alice, bob = ctx.alice, ctx.bob
    room_id = ctx.room_id
    bob.api.post(
        f"/api/rooms/{room_id}/messages",
        {
            "content": "paging alice",
            "msgHash": msg_hash("paging alice"),
            "mentions": [alice.address],
        },
    ).expect(200)

    mentions = alice.api.get("/api/mentions").expect(200).json()
    hit = next(m for m in mentions if m["roomId"] == room_id)
    check(hit["isUnread"] is True, f"unread mention {hit}")
    rooms = alice.api.get("/api/rooms").expect(200).json()
    check(
        next(r for r in rooms if r["id"] == room_id)["mentionCount"] >= 1,
        "mentionCount set",
    )

    serial = (
        alice.api.get(f"/api/rooms/{room_id}/latest-serial")
        .expect(200)
        .json()["serial"]
    )
    alice.api.post(f"/api/rooms/{room_id}/read", {"lastReadSerial": serial}).expect(200)
    rooms = alice.api.get("/api/rooms").expect(200).json()
    mine = next(r for r in rooms if r["id"] == room_id)
    check(
        mine["mentionCount"] == 0 and mine["unreadCount"] == 0,
        f"read clears counts {mine}",
    )

    # The pointer is monotonic: a lower value is a no-op.
    kept = (
        alice.api.post(f"/api/rooms/{room_id}/read", {"lastReadSerial": 1})
        .expect(200)
        .json()
    )
    check(kept["lastReadSerial"] == serial, f"monotonic read pointer {kept}")


def flow_presence(ctx):
    alice = ctx.alice
    alice.api.put("/api/presence", {"status": "away"}).expect(200)
    alice.api.put("/api/presence", {"status": "offline"}).expect(
        400, "offline is derived"
    )
    alice.api.put("/api/presence", {"status": "busy"}).expect(400)
    alice.api.request("PUT", "/api/presence", raw_body=b'{"status":"away"}').expect(
        415, "presence requires Content-Type: application/json"
    )
    listed = alice.api.get("/api/presence").expect(200).json()
    me = next(p for p in listed if p["walletAddress"] == alice.address)
    check(me["status"] == "away", f"beacon visible {listed}")


def flow_blocking(ctx):
    alice, dave = ctx.alice, ctx.dave
    alice.api.post("/api/users/block", {"address": dave.address}).expect(200)
    alice.api.post("/api/users/block", {"address": dave.address}).expect(
        200, "idempotent"
    )
    alice.api.post("/api/users/block", {"address": alice.address}).expect(
        400, "not yourself"
    )
    alice.api.post("/api/users/block", {"address": "junk"}).expect(400)

    blocked = alice.api.get("/api/users/blocked").expect(200).json()
    check(
        any(b["blockedAddress"] == dave.address for b in blocked),
        f"blocked list {blocked}",
    )
    check(
        alice.api.get(f"/api/users/{dave.address}/is-blocked")
        .expect(200)
        .json()["isBlocked"],
        "is-blocked true",
    )
    by = dave.api.get("/api/users/blocked-by").expect(200).json()
    check(
        any(b["blockerAddress"] == alice.address for b in by),
        "dave sees who blocked him",
    )

    hits = alice.api.get("/api/users/search?q=dave").expect(200).json()
    check(
        all(u["walletAddress"] != dave.address for u in hits),
        "search hides the blocked",
    )
    room_id = ctx.new_room(alice, "No daves allowed")
    alice.api.post(
        f"/api/rooms/{room_id}/invite", {"userAddress": dave.address}
    ).expect(403, "cannot invite the blocked")

    alice.api.delete(f"/api/users/block/{dave.address}").expect(200)
    alice.api.delete(f"/api/users/block/{dave.address}").expect(
        200, "unblock is idempotent"
    )
    alice.api.post(
        f"/api/rooms/{room_id}/invite", {"userAddress": dave.address}
    ).expect(200, "invitable again")


def flow_hide(ctx):
    alice = ctx.alice
    room_id = ctx.new_room(alice, "Hideout")
    alice.api.post(f"/api/rooms/{room_id}/hide").expect(200)
    rooms = alice.api.get("/api/rooms").expect(200).json()
    check(all(r["id"] != room_id for r in rooms), "hidden room left the list")
    hidden = alice.api.get("/api/rooms/hidden").expect(200).json()
    check(any(h["roomId"] == room_id for h in hidden), f"hidden list {hidden}")
    alice.api.delete(f"/api/rooms/{room_id}/hide").expect(200)
    rooms = alice.api.get("/api/rooms").expect(200).json()
    check(any(r["id"] == room_id for r in rooms), "unhidden room is back")
    ctx.bob.api.post(f"/api/rooms/{room_id}/hide").expect(403, "members only")


def flow_sync(ctx):
    alice = ctx.alice
    room_id = ctx.new_room(alice, "Sync lab")
    for i in range(5):
        alice.api.post(
            f"/api/rooms/{room_id}/messages",
            {
                "content": f"event {i}",
                "msgHash": msg_hash(f"event {i}"),
            },
        ).expect(200)
    deleted = alice.api.get(f"/api/rooms/{room_id}/messages").expect(200).json()[0]
    alice.api.delete(f"/api/messages/{deleted['id']}").expect(200)

    # Drain from zero: ascending serials, exclusive cursor, X-Has-More header,
    # and the delete arrives as a foldable event rather than vanishing.
    seen, since = [], 0
    while True:
        resp = alice.api.get(f"/api/rooms/{room_id}/sync?since={since}").expect(200)
        batch = resp.json()
        if not batch:
            check(resp.headers["X-Has-More"] == "false", "empty batch means done")
            break
        serials = [m["msgSerial"] for m in batch]
        check(serials == sorted(serials) and serials[0] > since, "ascending, exclusive")
        seen += batch
        since = serials[-1]
        if resp.headers["X-Has-More"] == "false":
            break
    # A soft delete rewrites the row in place (same id, msgType "delete",
    # fresh serial) rather than appending an event — still 5 rows.
    check(len(seen) == 5, f"5 rows, got {len(seen)}")
    check(
        sum(m["msgType"] == "delete" for m in seen) == 1, "deleted row present in sync"
    )

    latest = alice.api.get(f"/api/rooms/{room_id}/latest-serial").expect(200).json()
    check(latest["serial"] == seen[-1]["msgSerial"], "latest-serial matches the tail")


def flow_search_and_knowledge(ctx):
    alice = ctx.alice
    room_id = ctx.new_room(alice, "Library")
    token = "xylophone77"
    alice.api.post(
        f"/api/rooms/{room_id}/messages",
        {
            "content": f"the {token} is here",
            "msgHash": msg_hash("x"),
        },
    ).expect(200)

    found = []
    for _ in range(20):  # indexing may lag a moment behind the write
        found = (
            alice.api.get(f"/api/search?q={token}&kind=message")
            .expect(200)
            .json()["results"]
        )
        if found:
            break
        time.sleep(0.25)
    check(any(token in r["text"] for r in found), f"search finds the message {found}")
    check(
        ctx.carol.api.get(f"/api/search?q={token}&kind=message")
        .expect(200)
        .json()["results"]
        == [],
        "search respects room membership",
    )
    alice.api.get(f"/api/search?q={token}&kind=nonsense").expect(400)

    note = (
        alice.api.post("/api/knowledge", {"content": "keep this fact"})
        .expect(200)
        .json()
    )
    notes = alice.api.get("/api/knowledge?owner=me").expect(200).json()["notes"]
    check(any(n["id"] == note["id"] for n in notes), f"knowledge listed {notes}")
    ctx.bob.api.delete(f"/api/knowledge/{note['id']}").expect(403, "author only")
    alice.api.delete(f"/api/knowledge/{note['id']}").expect(200)


def flow_files(ctx):
    alice, bob = ctx.alice, ctx.bob
    room_id = ctx.room_id
    payload = bytes(range(256)) * 40  # 10 KB
    meta = (
        alice.api.post(
            f"/api/rooms/{room_id}/files?filename=blob.bin&caption=ten%20kb",
            raw_body=payload,
        )
        .expect(201, "whole-body upload")
        .json()
    )
    check(meta["sizeBytes"] == len(payload), f"file meta {meta}")
    fid = meta["id"]

    alice.api.post(
        f"/api/rooms/{room_id}/files?filename=empty.bin", raw_body=b""
    ).expect(400, "empty file")
    files = bob.api.get(f"/api/rooms/{room_id}/files").expect(200).json()["files"]
    check(any(f["id"] == fid for f in files), "member sees the file")
    ctx.carol.api.get(f"/api/files/{fid}").expect(404, "non-member gets a uniform 404")

    raw = bob.api.get(f"/api/files/{fid}/raw").expect(200)
    check(raw.body == payload, "download round-trips")
    check(raw.headers["X-Content-SHA256"] == sha256_hex(payload), "sha256 header")

    part = bob.api.get(f"/api/files/{fid}/raw", headers={"Range": "bytes=0-99"}).expect(
        206
    )
    check(part.body == payload[:100], "range slice")
    check(
        part.headers["Content-Range"] == f"bytes 0-99/{len(payload)}", "content-range"
    )
    bob.api.get(
        f"/api/files/{fid}/raw", headers={"Range": f"bytes={len(payload) + 10}-"}
    ).expect(416)

    grant = bob.api.get(f"/api/files/{fid}/download-token").expect(200).json()
    anon = Api(ctx.base_url)
    check(
        anon.get(grant["url"]).expect(200, "capability URL works without auth").body
        == payload,
        "capability download round-trips",
    )

    alice.api.delete(f"/api/files/{fid}").expect(200)
    bob.api.get(f"/api/files/{fid}").expect(404, "deleted")


def flow_chunked_upload(ctx):
    alice = ctx.alice
    room_id = ctx.room_id
    payload = bytes((i * 7 + 3) % 256 for i in range(100_000))
    session = (
        alice.api.post(
            "/api/uploads",
            {
                "kind": "file",
                "roomId": room_id,
                "filename": "big.bin",
                "size": len(payload),
                "sha256": sha256_hex(payload),
            },
        )
        .expect(201, "open upload session")
        .json()
    )
    uid = session["id"]
    check(session["offset"] == 0, f"session {session}")

    chunk = 40_000
    offset = 0
    while offset < len(payload):
        piece = payload[offset : offset + chunk]
        state = (
            alice.api.patch(f"/api/uploads/{uid}?offset={offset}", raw_body=piece)
            .expect(200)
            .json()
        )
        offset = state["offset"]
        if offset == chunk:  # replaying the first chunk must 409 with the real offset
            alice.api.patch(f"/api/uploads/{uid}?offset=0", raw_body=piece).expect(
                409, "offset mismatch"
            )

    status = alice.api.get(f"/api/uploads/{uid}").expect(200).json()
    check(status["offset"] == len(payload), f"complete {status}")
    meta = alice.api.post(f"/api/uploads/{uid}/finish").expect(201, "finish").json()
    check(meta["sizeBytes"] == len(payload), f"finished file {meta}")
    raw = alice.api.get(f"/api/files/{meta['id']}/raw").expect(200)
    check(raw.body == payload, "chunked upload round-trips")
    alice.api.delete(f"/api/files/{meta['id']}").expect(200)

    # A cancelled session leaves nothing behind.
    session = (
        alice.api.post(
            "/api/uploads",
            {
                "kind": "file",
                "roomId": room_id,
                "filename": "gone.bin",
                "size": 10,
            },
        )
        .expect(201)
        .json()
    )
    alice.api.delete(f"/api/uploads/{session['id']}").expect(204)
    alice.api.get(f"/api/uploads/{session['id']}").expect(404)


# A valid 1x1 transparent PNG.
_PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c489"
    "0000000d49444154789c626001000000ffff03000006000557bfabd40000000049454e44ae426082"
)


def flow_images(ctx):
    alice = ctx.alice
    alice.api.post(
        "/api/images", raw_body=_PNG, headers={"Content-Type": "text/plain"}
    ).expect(400, "unsupported type")
    url = (
        alice.api.post(
            "/api/images", raw_body=_PNG, headers={"Content-Type": "image/png"}
        )
        .expect(200)
        .json()["url"]
    )
    check(url.startswith("/api/images/"), f"hosted url {url}")
    img = Api(ctx.base_url).get(url).expect(200, "images are public")
    check(img.body == _PNG, "image round-trips")
    check("immutable" in img.headers["Cache-Control"], "content-addressed caching")

    # Profile updates always carry the username; profileImage rides along.
    alice.api.put(
        "/api/auth/profile", {"username": "alice", "profileImage": url}
    ).expect(200)
    alice.api.put(
        "/api/auth/profile",
        {"username": "alice", "profileImage": "javascript:alert(1)"},
    ).expect(400)
    alice.api.put(
        "/api/auth/profile", {"username": "alice", "profileImage": ""}
    ).expect(200, "clear it")


def flow_e2ee_keys(ctx):
    alice, bob = ctx.alice, ctx.bob

    def publish_key(user):
        derivation = (
            "FruitNation Encryption Key Derivation v2\n\n"
            f"Address: {user.address}\nSalt: {user.encryption_salt}\n"
            "Purpose: End-to-end encryption only"
        )
        sig = user.wallet.personal_sign(derivation)
        enc_priv = int.from_bytes(keccak256(bytes.fromhex(sig[2:])), "big")
        enc_pub = Wallet(enc_priv).pubkey_bytes.hex()  # "04" + x + y, no 0x
        binding = (
            "FruitNation Public Key Binding\n\n"
            f"Address: {user.address}\nEncryption Public Key: {enc_pub}"
        )
        body = (
            user.api.put(
                "/api/auth/encryption-key",
                {
                    "publicKey": enc_pub,
                    "publicKeySig": user.wallet.personal_sign(binding),
                },
            )
            .expect(200, "publish encryption key")
            .json()
        )
        check(set(body) == {"walletAddress", "publicKey"}, f"response shape {body}")
        return enc_pub

    alice_pub = publish_key(alice)
    bob_pub = publish_key(bob)
    alice.api.put(
        "/api/auth/encryption-key",
        {
            "publicKey": alice_pub,
            "publicKeySig": "0x" + "ab" * 65,
        },
    ).expect(400, "binding signature is verified")

    keys = (
        alice.api.post("/api/users/public-keys", {"addresses": [bob.address]})
        .expect(200)
        .json()
    )
    check(keys[0]["publicKey"] == bob_pub, f"published key served {keys}")

    salt = alice.api.get("/api/auth/encryption-salt").expect(200).json()["salt"]
    check(salt == alice.encryption_salt, "salt is stable")

    # Room-key storage and rotation, with wrapped-key payloads of valid shape.
    room_id = ctx.new_room(alice, "Vault")
    ctx.add_member(room_id, alice, bob)

    def wrapped(user_address):
        return {
            "userAddress": user_address,
            "encryptedSymmetricKey": "00" * 48,
            "ephemeralPublicKey": "04" + "11" * 64,
            "encryptionIV": "22" * 16,
            "hmac": "33" * 32,
            "encVer": 2,
        }

    alice.api.post(
        f"/api/rooms/{room_id}/keys", {**wrapped(alice.address), "keyVersion": 1}
    ).expect(200)
    bob.api.post(
        f"/api/rooms/{room_id}/keys", {**wrapped(alice.address), "keyVersion": 1}
    ).expect(403, "only admins store keys for others")
    check(
        alice.api.get(f"/api/rooms/{room_id}/keys").expect(200).json()["keyVersion"]
        == 1,
        "stored key readable",
    )

    alice.api.post(
        f"/api/rooms/{room_id}/rotate-key",
        {
            "newVersion": 2,
            "keys": [wrapped(alice.address)],
        },
    ).expect(400, "rotation must cover every member")
    rotated = (
        alice.api.post(
            f"/api/rooms/{room_id}/rotate-key",
            {
                "newVersion": 2,
                "keys": [wrapped(alice.address), wrapped(bob.address)],
            },
        )
        .expect(200)
        .json()
    )
    check(rotated["newVersion"] == 2, f"rotated {rotated}")
    versions = bob.api.get(f"/api/rooms/{room_id}/keys/versions").expect(200).json()
    check(
        [v["keyVersion"] for v in versions] == [2],
        f"bob has only the new epoch {versions}",
    )


def flow_realtime_sse(ctx):
    alice, bob = ctx.alice, ctx.bob
    room_id = ctx.room_id
    ticket = alice.api.post("/api/events/ticket").expect(200).json()["ticket"]
    stream = alice.api.open_stream(f"/api/events?ticket={ticket}", timeout=15)
    check(
        stream.headers["Content-Type"].startswith("text/event-stream"),
        "SSE content type",
    )
    try:
        bob.api.post(
            f"/api/rooms/{room_id}/messages",
            {
                "content": "wake up",
                "msgHash": msg_hash("wake up"),
            },
        ).expect(200)
        deadline = time.monotonic() + 15
        saw = False
        while time.monotonic() < deadline:
            line = stream.readline().decode("utf-8", "replace").strip()
            if line.startswith("event:") and "new_message" in line:
                data = stream.readline().decode("utf-8", "replace").strip()
                check(room_id in data, f"event targets the room: {data}")
                saw = True
                break
        check(saw, "new_message arrived over SSE")
    finally:
        stream.close()

    # A ticket is single-use.
    alice.api.get(f"/api/events?ticket={ticket}").expect(401, "ticket burned")
    # And ?token= is refused without --sse-token-query.
    alice.api.get(f"/api/events?token={alice.api.token}", token=None).expect(401)


def flow_admin(ctx):
    admin, alice, dave = ctx.admin, ctx.alice, ctx.dave
    alice.api.get("/api/admin/overview").expect(403, "mortals get a 403")
    check(
        alice.api.get("/api/admin/session").expect(200).json()["isServerAdmin"]
        is False,
        "session says no",
    )
    check(
        admin.api.get("/api/admin/session").expect(200).json()["isServerAdmin"] is True,
        "session says yes",
    )

    overview = admin.api.get("/api/admin/overview").expect(200).json()
    check(admin.address in overview["admins"], f"overview {overview}")
    users = admin.api.get("/api/admin/users").expect(200).json()
    check(
        any(u.get("walletAddress") == dave.address for u in users),
        "dave is on the roster",
    )

    # Suspension bites the existing token, and reinstatement lifts it.
    admin.api.post(
        f"/api/admin/users/{dave.address}/suspend", {"reason": "integration test"}
    ).expect(200)
    dave.api.get("/api/auth/profile").expect(401, "suspended token refused")
    admin.api.delete(f"/api/admin/users/{dave.address}/suspend").expect(200)
    dave.api.get("/api/auth/profile").expect(200, "reinstated")

    admin.api.post(f"/api/admin/users/{admin.address}/suspend").expect(
        400, "cannot suspend yourself"
    )

    # A server admin passes room-admin checks on rooms they never joined.
    room_id = ctx.new_room(alice, "Under inspection")
    admin.api.patch(f"/api/rooms/{room_id}", {"name": "Inspected"}).expect(200)
    admin.api.delete(f"/api/admin/rooms/{room_id}").expect(200)
    alice.api.get(f"/api/rooms/{room_id}").expect(403, "room removed by the admin")


def flow_destroy_room(ctx):
    alice = ctx.alice
    room_id = ctx.new_room(alice, "Doomed")
    alice.api.post(
        f"/api/rooms/{room_id}/messages",
        {"content": "last words", "msgHash": msg_hash("last words")},
    ).expect(200)
    fid = (
        alice.api.post(
            f"/api/rooms/{room_id}/files?filename=doomed.bin", raw_body=b"payload"
        )
        .expect(201)
        .json()["id"]
    )

    gone = alice.api.delete(f"/api/rooms/{room_id}").expect(200).json()
    check(gone["purged"]["attachments"] >= 1, f"bytes purged with the room {gone}")
    alice.api.get(f"/api/rooms/{room_id}").expect(403, "room gone")
    alice.api.get(f"/api/files/{fid}").expect(404, "its files gone too")


# --- TLS + HTTP/3 flows -----------------------------------------------------
#
# These run against a second backend booted with `--tls --http3`: the server
# mints its own self-signed certificate on the fly, and the suite trusts
# exactly that CA — never the system store, never an unverified handshake.
# The full QUIC request exchange is the Rust http3.rs suite's job; here we
# prove the deployment shape a real client meets: a verifiable certificate,
# the app working over HTTPS, HTTP/3 advertised, and a live QUIC listener.


class TlsContext:
    def __init__(self, backend):
        self.backend = backend
        self.base_url = backend.base_url
        self.ca = backend.ca_path


def tls_flow_certificate(ctx):
    api = Api(ctx.base_url, ca_file=ctx.ca)
    api.get("/api/health").expect(200, "HTTPS with the minted CA trusted")

    try:
        Api(ctx.base_url).get("/api/health")
        raise AssertionError(
            "a self-signed cert must not verify against the system store"
        )
    except urllib.error.URLError:
        pass  # CERTIFICATE_VERIFY_FAILED is the point

    served = api.get("/ca.crt").expect(200, "the server hands out its own CA").body
    with open(ctx.ca, "rb") as ca_file:
        check(served == ca_file.read(), "/ca.crt matches the CA on disk")


def tls_flow_full_flow(ctx):
    # The app itself, not just the handshake, over the self-signed TLS.
    user = login(ctx.base_url, Wallet(), "tls_user", ca_file=ctx.ca)
    room = user.api.post("/api/rooms", {"name": "Encrypted transit"}).expect(200).json()
    user.api.post(
        f"/api/rooms/{room['id']}/messages",
        {
            "content": "over https",
            "msgHash": msg_hash("over https"),
        },
    ).expect(200)
    listed = user.api.get(f"/api/rooms/{room['id']}/messages").expect(200).json()
    check(listed[0]["content"] == "over https", "message round-trips over TLS")


def tls_flow_http3_advertised(ctx):
    api = Api(ctx.base_url, ca_file=ctx.ca)
    resp = api.get("/api/server/info").expect(200)
    info = resp.json()
    check(info["scheme"] == "https", f"scheme {info}")
    check(info["http3Available"] is True, f"http3 advertised {info}")
    check(info["http3Port"] == ctx.backend.http3_port, f"http3 port {info}")

    alt_svc = resp.headers.get("Alt-Svc", "")
    check(
        f'h3=":{ctx.backend.http3_port}"' in alt_svc,
        f"Alt-Svc advertises the QUIC port: {alt_svc!r}",
    )


def tls_flow_quic_listener(ctx):
    # A QUIC hello without a QUIC stack: send a long-header packet carrying a
    # reserved GREASE version (0x?a?a?a?a forces negotiation, RFC 9000 §6),
    # padded to the 1200-byte minimum. A live listener must answer with a
    # Version Negotiation packet — version 0, real versions listed after.
    dcid, scid = secrets.token_bytes(8), secrets.token_bytes(8)
    header = (
        bytes([0xC0])
        + b"\x1a\x2a\x3a\x4a"
        + bytes([len(dcid)])
        + dcid
        + bytes([len(scid)])
        + scid
    )
    packet = header + b"\x00" * (1200 - len(header))

    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.settimeout(10)
        sock.sendto(packet, ("127.0.0.1", ctx.backend.http3_port))
        data, _ = sock.recvfrom(2048)

    check(data[0] & 0x80, "long header in the reply")
    check(data[1:5] == b"\x00\x00\x00\x00", "a Version Negotiation packet")
    check(b"\x00\x00\x00\x01" in data[5:], "QUIC v1 among the offered versions")


def tls_flow_http_redirect(ctx):
    # The plain-HTTP listener beside a TLS server exists to redirect, not to
    # serve; a client-side redirect follow is exactly what must NOT happen
    # here, so this speaks http.client directly and inspects the 3xx.
    conn = http.client.HTTPConnection(
        "127.0.0.1", ctx.backend.redirect_port, timeout=10
    )
    try:
        conn.request("GET", "/api/health")
        resp = conn.getresponse()
        check(resp.status in (301, 302, 307, 308), f"redirect status {resp.status}")
        location = resp.getheader("Location") or ""
        check(location.startswith("https://"), f"redirects to HTTPS: {location!r}")
    finally:
        conn.close()


TLS_FLOWS = [
    tls_flow_certificate,
    tls_flow_full_flow,
    tls_flow_http3_advertised,
    tls_flow_quic_listener,
    tls_flow_http_redirect,
]


FLOWS = [
    flow_health_and_info,
    flow_auth,
    flow_profile_and_users,
    flow_rooms,
    flow_invitations,
    flow_admins_and_kick,
    flow_leave,
    flow_dm,
    flow_messages,
    flow_emoticons,
    flow_mentions_and_read,
    flow_presence,
    flow_blocking,
    flow_hide,
    flow_sync,
    flow_search_and_knowledge,
    flow_files,
    flow_chunked_upload,
    flow_images,
    flow_e2ee_keys,
    flow_realtime_sse,
    flow_admin,
    flow_destroy_room,
]
