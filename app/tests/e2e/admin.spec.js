const { test, expect } = require('@playwright/test');
const { signIn, forget, api, hash, json, channel, join, post } = require('./helpers');

// This server is started with VITE_FRUITNATION_ADMIN set to the `boss` wallet
// and nothing else, so every assertion below exercises the real env-driven
// path rather than a test hook.

test.describe('server administration', () => {
  test('the login response and /admin/session agree on who is an admin', async ({ request }) => {
    const boss = await signIn(request, 'boss');
    const alice = await signIn(request, 'ad-who-alice');

    expect(boss.isServerAdmin).toBe(true);
    expect(alice.isServerAdmin).toBe(false);

    // A client restoring a stored token has the token but not the login
    // response, so it has to be able to ask — and the question must be
    // answerable for somebody who is *not* an admin.
    expect((await json(await api(request, boss).get('/api/admin/session'))).isServerAdmin).toBe(
      true,
    );
    const asAlice = await api(request, alice).get('/api/admin/session');
    expect(asAlice.status()).toBe(200);
    expect((await asAlice.json()).isServerAdmin).toBe(false);
  });

  test('every admin route refuses a non-admin', async ({ request }) => {
    const alice = await signIn(request, 'ad-gate-alice');
    for (const path of ['/api/admin/overview', '/api/admin/users', '/api/admin/rooms']) {
      expect((await api(request, alice).get(path)).status(), path).toBe(403);
    }
    expect(
      (await api(request, alice).post(`/api/admin/users/${alice.address}/suspend`)).status(),
    ).toBe(403);
    expect((await api(request, alice).delete('/api/admin/rooms/room_1_x')).status()).toBe(403);

    // And an unauthenticated caller gets 401, not a hint that the route exists.
    expect((await api(request, null).get('/api/admin/overview')).status()).toBe(401);
  });

  test('the overview counts the server and echoes the configured admin list', async ({
    request,
  }) => {
    const boss = await signIn(request, 'boss');
    const alice = await signIn(request, 'ad-view-alice');
    await api(request, alice).post('/api/rooms', { name: 'Ops' });
    await api(request, alice).post('/api/rooms/dm', { walletAddress: boss.address });

    const overview = await json(await api(request, boss).get('/api/admin/overview'));
    expect(overview.totals.users).toBeGreaterThanOrEqual(2);
    expect(overview.totals.channels).toBeGreaterThanOrEqual(1);
    expect(overview.totals.directMessages).toBeGreaterThanOrEqual(1);
    // A mistyped address in VITE_FRUITNATION_ADMIN is otherwise completely
    // silent; this is the one place an operator can see what was parsed.
    expect(overview.admins).toEqual([boss.address.toLowerCase()]);

    const users = await json(await api(request, boss).get('/api/admin/users'));
    const bossRow = users.find((u) => u.walletAddress === boss.address);
    const aliceRow = users.find((u) => u.walletAddress === alice.address);
    expect(bossRow.isServerAdmin).toBe(true);
    expect(aliceRow.isServerAdmin).toBe(false);
    expect(aliceRow.roomCount).toBeGreaterThanOrEqual(1);

    const rooms = await json(await api(request, boss).get('/api/admin/rooms'));
    const ops = rooms.find((r) => r.name === 'Ops');
    expect(ops.kind).toBe('channel');
    expect(ops.memberCount).toBe(1);
    // Metadata only: an admin can see a room exists, never read it.
    expect(ops.content).toBeUndefined();
    expect(ops.lastMessage).toBeUndefined();
  });

  test('suspending invalidates a token that was already issued', async ({ request }) => {
    const boss = await signIn(request, 'boss');
    const carol = await signIn(request, 'ad-susp-carol');

    expect((await api(request, carol).get('/api/rooms')).status()).toBe(200);

    const suspended = await api(request, boss).post(
      `/api/admin/users/${carol.address}/suspend`,
      { reason: 'posting from a compromised laptop' },
    );
    expect(suspended.status(), await suspended.text()).toBe(200);

    // The same token, unchanged, now fails. There is no revocation list, so
    // the decision is remade every request — that is the whole mechanism.
    expect((await api(request, carol).get('/api/rooms')).status()).toBe(401);

    // And signing in again does not get around it.
    await expect(signIn(request, 'ad-susp-carol', { fresh: true })).rejects.toThrow(/403/);

    const users = await json(await api(request, boss).get('/api/admin/users'));
    const row = users.find((u) => u.walletAddress === carol.address);
    expect(row.isSuspended).toBe(true);
    expect(row.suspendedReason).toBe('posting from a compromised laptop');

    // Reinstating restores both.
    expect(
      (await api(request, boss).delete(`/api/admin/users/${carol.address}/suspend`)).status(),
    ).toBe(200);
    expect((await api(request, carol).get('/api/rooms')).status()).toBe(200);
    forget('ad-susp-carol');
    const back = await signIn(request, 'ad-susp-carol', { fresh: true });
    expect(back.token).toBeTruthy();
  });

  test('an admin cannot suspend or remove themselves', async ({ request }) => {
    const boss = await signIn(request, 'boss');
    expect((await api(request, boss).post(`/api/admin/users/${boss.address}/suspend`)).status())
      .toBe(400);
    expect((await api(request, boss).delete(`/api/admin/users/${boss.address}`)).status()).toBe(
      400,
    );
    // Still working afterwards — no half-applied state.
    expect((await api(request, boss).get('/api/admin/overview')).status()).toBe(200);
  });

  test('removing someone evicts them everywhere and flags a re-key', async ({ request }) => {
    const boss = await signIn(request, 'boss');
    const alice = await signIn(request, 'ad-evict-alice');
    const carol = await signIn(request, 'ad-evict-carol');

    const room = (
      await json(await api(request, alice).post('/api/rooms', { name: 'Payroll' }))
    ).id;
    await api(request, alice).post(`/api/rooms/${room}/invite`, { userAddress: carol.address });
    await api(request, carol).post(`/api/invitations/${room}/accept`);
    await api(request, carol).post(`/api/rooms/${room}/messages`, {
      content: 'here is the spreadsheet',
      msgHash: hash('a'),
    });

    const removed = await api(request, boss).delete(`/api/admin/users/${carol.address}`);
    expect(removed.status(), await removed.text()).toBe(200);

    expect((await api(request, carol).get('/api/rooms')).status()).toBe(401);

    const rooms = await json(await api(request, alice).get('/api/rooms'));
    const row = rooms.find((r) => r.id === room);
    expect(row.memberCount).toBe(1);
    // Carol may still hold the room key, so nothing may be sealed under it
    // until Alice rotates — the same guarantee leaving gives.
    expect(row.keyRotationPending).toBe(true);

    // Her history stays, attributed to her: the room's record of a
    // conversation is not the operator's to rewrite.
    const listed = await json(await api(request, alice).get(`/api/rooms/${room}/messages`));
    const hers = listed.find((m) => m.senderAddress === carol.address);
    expect(hers.content).toBe('here is the spreadsheet');
    expect(hers.sender.username).toBe(carol.username);

    await api(request, boss).delete(`/api/admin/users/${carol.address}/suspend`);
    forget('ad-evict-carol');
  });

  test('an admin can manage and delete a room they were never in', async ({ request }) => {
    const boss = await signIn(request, 'boss');
    const alice = await signIn(request, 'ad-room-alice');
    const room = (
      await json(await api(request, alice).post('/api/rooms', { name: 'Abandoned' }))
    ).id;

    // Room-admin powers without membership: this is what makes a room whose
    // last admin left recoverable rather than permanent.
    const renamed = await api(request, boss).patch(`/api/rooms/${room}`, { name: 'Reclaimed' });
    expect(renamed.status(), await renamed.text()).toBe(200);

    const deleted = await api(request, boss).delete(`/api/admin/rooms/${room}`);
    expect(deleted.status(), await deleted.text()).toBe(200);

    const rooms = await json(await api(request, alice).get('/api/rooms'));
    expect(rooms.find((r) => r.id === room)).toBeUndefined();

    // Deleting it twice is a 404, not a 500.
    expect((await api(request, boss).delete(`/api/admin/rooms/${room}`)).status()).toBe(404);
  });

  test('purging a room history is admin-only', async ({ request }) => {
    const boss = await signIn(request, 'boss');
    const alice = await signIn(request, 'ad-purge-alice');
    const bob = await signIn(request, 'ad-purge-bob');

    const room = (await json(await api(request, alice).post('/api/rooms', { name: 'History' })))
      .id;
    await api(request, alice).post(`/api/rooms/${room}/invite`, { userAddress: bob.address });
    await api(request, bob).post(`/api/invitations/${room}/accept`);
    const message = await json(
      await api(request, alice).post(`/api/rooms/${room}/messages`, {
        content: 'worth keeping',
        msgHash: hash('a'),
      }),
    );

    // Bob is a member and may still delete a single message...
    const refused = await api(request, bob).delete(`/api/rooms/${room}/messages`);
    expect(refused.status()).toBe(403);
    expect((await refused.json()).message).toContain('Only room admins');
    expect((await api(request, bob).delete(`/api/messages/${message.id}`)).status()).toBe(200);

    // ...but erasing everybody's record in one request is the admin's.
    expect((await api(request, alice).delete(`/api/rooms/${room}/messages`)).status()).toBe(200);
    expect((await api(request, boss).delete(`/api/rooms/${room}/messages`)).status()).toBe(200);
  });
});
