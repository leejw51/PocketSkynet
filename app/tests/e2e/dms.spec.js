const { test, expect } = require("@playwright/test");
const { signIn, api, json, post } = require("./helpers");

test.describe("direct messages", () => {
  test("opening the same DM from either side lands in one room", async ({
    request,
  }) => {
    const alice = await signIn(request, "dm-pair-alice");
    const bob = await signIn(request, "dm-pair-bob");

    // Sent checksummed, the way a wallet or a person would write it: the
    // server normalises, and the DM key must not fork on casing.
    const opened = await api(request, alice).post("/api/rooms/dm", {
      walletAddress: bob.checksummed,
    });
    expect(opened.status(), await opened.text()).toBe(200);
    const room = await opened.json();
    expect(room.kind).toBe("dm");
    // Enriched, so the client can name the DM after its other member without
    // a second request.
    expect(room.memberCount).toBe(2);
    expect(room.members.map((m) => m.userAddress).sort()).toEqual(
      [alice.address, bob.address].sort(),
    );

    // Bob opening "the conversation with Alice" must find hers.
    const fromBob = await api(request, bob).post("/api/rooms/dm", {
      walletAddress: alice.address,
    });
    expect((await fromBob.json()).id).toBe(room.id);

    // A third call is still the same room, not a third one.
    const again = await api(request, alice).post("/api/rooms/dm", {
      walletAddress: bob.address,
    });
    expect((await again.json()).id).toBe(room.id);

    for (const who of [alice, bob]) {
      const rooms = await json(await api(request, who).get("/api/rooms"));
      expect(rooms.filter((r) => r.id === room.id)).toHaveLength(1);
    }
  });

  test("three people get a group DM, distinct from any pair", async ({
    request,
  }) => {
    const alice = await signIn(request, "dm-group-alice");
    const bob = await signIn(request, "dm-group-bob");
    const carol = await signIn(request, "dm-group-carol");

    const group = await json(
      await api(request, alice).post("/api/rooms/dm", {
        walletAddresses: [bob.address, carol.address],
      }),
    );
    expect(group.kind).toBe("group_dm");
    expect(group.memberCount).toBe(3);

    const pair = await json(
      await api(request, alice).post("/api/rooms/dm", {
        walletAddress: carol.address,
      }),
    );
    expect(pair.kind).toBe("dm");
    expect(pair.id).not.toBe(group.id);

    // Order of the recipient list cannot fork the conversation, and neither
    // can which member asks.
    const reordered = await json(
      await api(request, carol).post("/api/rooms/dm", {
        walletAddresses: [alice.address, bob.address],
      }),
    );
    expect(reordered.id).toBe(group.id);
  });

  test("a DM refuses every verb that only makes sense for a channel", async ({
    request,
  }) => {
    const alice = await signIn(request, "dm-verbs-alice");
    const bob = await signIn(request, "dm-verbs-bob");
    const carol = await signIn(request, "dm-verbs-carol");
    const room = await json(
      await api(request, alice).post("/api/rooms/dm", {
        walletAddress: bob.address,
      }),
    );

    const a = api(request, alice);
    // Alice is an admin of this DM, so none of these fail for permission —
    // they fail because the verb does not apply.
    expect(
      (await a.patch(`/api/rooms/${room.id}`, { name: "Renamed" })).status(),
    ).toBe(400);
    expect(
      (
        await a.post(`/api/rooms/${room.id}/invite`, {
          userAddress: carol.address,
        })
      ).status(),
    ).toBe(400);
    expect(
      (
        await a.post(`/api/rooms/${room.id}/kick`, { userAddress: bob.address })
      ).status(),
    ).toBe(400);
    expect(
      (
        await a.post(`/api/rooms/${room.id}/admins`, {
          walletAddress: bob.address,
        })
      ).status(),
    ).toBe(400);

    const left = await a.post(`/api/rooms/${room.id}/leave`);
    expect(left.status()).toBe(400);
    expect((await left.json()).message).toContain("Hide it instead");

    // Hiding is the verb offered instead, and it is reversible.
    expect((await a.post(`/api/rooms/${room.id}/hide`)).status()).toBe(200);
    let rooms = await json(await a.get("/api/rooms"));
    expect(rooms.find((r) => r.id === room.id)).toBeUndefined();
    expect((await a.delete(`/api/rooms/${room.id}/hide`)).status()).toBe(200);
    rooms = await json(await a.get("/api/rooms"));
    expect(rooms.find((r) => r.id === room.id)).toBeDefined();
  });

  test("a DM to a wallet that has never signed in is refused", async ({
    request,
  }) => {
    const alice = await signIn(request, "dm-stranger-alice");
    const response = await api(request, alice).post("/api/rooms/dm", {
      walletAddress: "0x1111111111111111111111111111111111111111",
    });
    // A mistyped address is far likelier than an invitation to somebody who
    // has genuinely never arrived.
    expect(response.status()).toBe(404);
  });

  test("naming only yourself opens a private notebook", async ({ request }) => {
    const carol = await signIn(request, "dm-self-carol");
    const room = await json(
      await api(request, carol).post("/api/rooms/dm", {
        walletAddress: carol.address,
      }),
    );
    expect(room.kind).toBe("dm");
    expect(room.memberCount).toBe(1);

    // And it works as a room: messages land and come back.
    await post(request, carol, room.id, "remember to rotate the keys");
    const listed = await json(
      await api(request, carol).get(`/api/rooms/${room.id}/messages`),
    );
    expect(listed.map((m) => m.content)).toEqual([
      "remember to rotate the keys",
    ]);
  });

  test("a DM is private to its members", async ({ request }) => {
    const alice = await signIn(request, "dm-private-alice");
    const bob = await signIn(request, "dm-private-bob");
    const outsider = await signIn(request, "dm-private-outsider");
    const room = await json(
      await api(request, alice).post("/api/rooms/dm", {
        walletAddress: bob.address,
      }),
    );
    await post(request, alice, room.id, "between us");

    expect(
      (
        await api(request, outsider).get(`/api/rooms/${room.id}/messages`)
      ).status(),
    ).toBe(403);
    expect(
      (await api(request, outsider).get(`/api/rooms/${room.id}`)).status(),
    ).toBe(403);
  });

  test("an ordinary channel still reports itself as one", async ({
    request,
  }) => {
    const alice = await signIn(request, "dm-channel-alice");
    const created = await json(
      await api(request, alice).post("/api/rooms", { name: "Engineering" }),
    );
    expect(created.kind).toBe("channel");
  });
});
