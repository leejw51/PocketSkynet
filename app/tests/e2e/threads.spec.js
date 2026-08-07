const { test, expect } = require("@playwright/test");
const { signIn, api, hash, json, channel, post } = require("./helpers");

test.describe("threads", () => {
  test("a thread costs the channel one line and reports its size", async ({
    request,
  }) => {
    const alice = await signIn(request, "th-size-alice");
    const room = await channel(request, alice, "Threads");

    const root = await post(request, alice, room, "shipping today?");
    expect(root.parentMessageId).toBeNull();
    expect(root.replyCount).toBeUndefined();

    for (const text of ["checking", "two tests left", "green"]) {
      const reply = await post(request, alice, room, text, {
        parentMessageId: root.id,
      });
      expect(reply.parentMessageId).toBe(root.id);
    }
    await post(request, alice, room, "unrelated");

    const listed = await json(
      await api(request, alice).get(`/api/rooms/${room}/messages`),
    );
    expect(listed.map((m) => m.content)).toEqual([
      "shipping today?",
      "unrelated",
    ]);
    const rootRow = listed.find((m) => m.id === root.id);
    expect(rootRow.replyCount).toBe(3);
    expect(rootRow.lastReplyAt).toBeGreaterThan(0);

    // A client with no thread UI can still ask for everything.
    const all = await json(
      await api(request, alice).get(
        `/api/rooms/${room}/messages?includeReplies=true`,
      ),
    );
    expect(all).toHaveLength(5);

    // The thread itself: root first, replies in the order they were sent.
    const thread = await json(
      await api(request, alice).get(`/api/messages/${root.id}/thread`),
    );
    expect(thread.map((m) => m.content)).toEqual([
      "shipping today?",
      "checking",
      "two tests left",
      "green",
    ]);
  });

  test("replies posted in one burst keep their order", async ({ request }) => {
    // The regression that motivated replacing the `m.id` tiebreak: ids are
    // `msg_{millis}_{uuid}`, so same-millisecond messages used to come back in
    // a random order. Twelve replies as fast as the server will take them.
    const alice = await signIn(request, "th-burst-alice");
    const room = await channel(request, alice, "Burst");
    const root = await post(request, alice, room, "root");

    const expected = [];
    for (let i = 0; i < 12; i++) {
      expected.push(`reply ${i}`);
      await post(request, alice, room, `reply ${i}`, {
        parentMessageId: root.id,
      });
    }

    const thread = await json(
      await api(request, alice).get(`/api/messages/${root.id}/thread`),
    );
    expect(thread.slice(1).map((m) => m.content)).toEqual(expected);
  });

  test("replying to a reply joins its thread rather than nesting", async ({
    request,
  }) => {
    const alice = await signIn(request, "th-flat-alice");
    const room = await channel(request, alice, "Flat");
    const root = await post(request, alice, room, "root");
    const first = await post(request, alice, room, "first", {
      parentMessageId: root.id,
    });
    const second = await post(request, alice, room, "second", {
      parentMessageId: first.id,
    });

    expect(second.parentMessageId).toBe(root.id);

    // Asking for the thread of any member of it answers the same list, so a
    // client holding only a reply from /sync can still open it.
    const fromReply = await json(
      await api(request, alice).get(`/api/messages/${first.id}/thread`),
    );
    expect(fromReply[0].id).toBe(root.id);
    expect(fromReply).toHaveLength(3);
  });

  test("a reply cannot cross into another room", async ({ request }) => {
    const alice = await signIn(request, "th-cross-alice");
    const publicRoom = await channel(request, alice, "Public");
    const privateRoom = await channel(request, alice, "Private");
    const elsewhere = await post(request, alice, privateRoom, "secret");

    const crossed = await api(request, alice).post(
      `/api/rooms/${publicRoom}/messages`,
      {
        content: "leaking",
        msgHash: hash("c"),
        parentMessageId: elsewhere.id,
      },
    );
    expect(crossed.status()).toBe(400);

    const orphan = await api(request, alice).post(
      `/api/rooms/${publicRoom}/messages`,
      {
        content: "orphan",
        msgHash: hash("d"),
        parentMessageId: "msg_1749652746620_ffffffff",
      },
    );
    expect(orphan.status()).toBe(404);
  });

  test("deleting the root keeps the thread together", async ({ request }) => {
    const alice = await signIn(request, "th-tomb-alice");
    const room = await channel(request, alice, "Tombstone");
    const root = await post(request, alice, room, "root");
    await post(request, alice, room, "still here", {
      parentMessageId: root.id,
    });

    expect(
      (await api(request, alice).delete(`/api/messages/${root.id}`)).status(),
    ).toBe(200);

    const thread = await json(
      await api(request, alice).get(`/api/messages/${root.id}/thread`),
    );
    expect(thread).toHaveLength(2);
    expect(thread[0].isDeleted).toBe(true);
    expect(thread[0].content).toBe("");
    expect(thread[1].content).toBe("still here");
  });

  test("a thread is not a way into a room", async ({ request }) => {
    const alice = await signIn(request, "th-access-alice");
    const carol = await signIn(request, "th-access-carol");
    const room = await channel(request, alice, "Internal");
    const root = await post(request, alice, room, "internal");

    const response = await api(request, carol).get(
      `/api/messages/${root.id}/thread`,
    );
    expect(response.status()).toBe(403);
  });

  test("replies still travel through /sync so a client can fold them itself", async ({
    request,
  }) => {
    const alice = await signIn(request, "th-sync-alice");
    const room = await channel(request, alice, "Sync");
    const root = await post(request, alice, room, "root");
    const reply = await post(request, alice, room, "reply", {
      parentMessageId: root.id,
    });

    const synced = await json(
      await api(request, alice).get(`/api/rooms/${room}/sync?since=0`),
    );
    const syncedReply = synced.find((m) => m.id === reply.id);
    expect(
      syncedReply,
      "a reply must reach /sync or offline clients lose it",
    ).toBeDefined();
    expect(syncedReply.parentMessageId).toBe(root.id);
  });
});
