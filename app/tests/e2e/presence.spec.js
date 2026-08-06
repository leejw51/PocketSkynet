// Presence, against a real server over real HTTP (API.md §6.15).
//
// The unit suite drives the hub directly and can hold a `ConnHandle` in its
// hand. What it cannot do is prove that *opening a stream* makes somebody
// appear and *closing it* makes them disappear, because that path runs through
// axum's connection lifecycle and a spawned task. That is what this file is
// for — and it is the half of presence most likely to break silently, since
// nothing about it fails loudly when it stops working. Everybody just quietly
// looks offline forever.

const { test, expect } = require('@playwright/test');
const { BASE, signIn, api, json, dm } = require('./helpers');

/** Everyone the caller can currently see, as `{address: status}`. */
async function seen(request, user) {
  const rows = await json(await api(request, user).get('/api/presence'));
  return Object.fromEntries(rows.map((r) => [r.walletAddress, r.status]));
}

test.describe('presence', () => {
  test('a beacon makes you visible to a room-mate and to nobody else', async ({ request }) => {
    const alice = await signIn(request, 'presence-beacon-alice');
    const bob = await signIn(request, 'presence-beacon-bob');
    const stranger = await signIn(request, 'presence-beacon-stranger');

    for (const who of [alice, bob, stranger]) {
      const declared = await api(request, who).put('/api/presence', { status: 'online' });
      expect(declared.status(), await declared.text()).toBe(200);
      expect((await declared.json()).status).toBe('online');
    }

    // No shared room yet: each sees only themselves. Presence is not a
    // directory — a shared room is the whole of what entitles you to it.
    expect(await seen(request, alice)).toEqual({ [alice.address]: 'online' });

    await dm(request, alice, bob);

    const now = await seen(request, alice);
    expect(now[bob.address]).toBe('online');
    expect(now[alice.address]).toBe('online');
    expect(now[stranger.address]).toBeUndefined();
  });

  test('away is a status a client may declare; offline is not', async ({ request }) => {
    const alice = await signIn(request, 'presence-away-alice');

    await api(request, alice).put('/api/presence', { status: 'away' });
    expect((await seen(request, alice))[alice.address]).toBe('away');

    // Claiming to be offline over a connection you are visibly holding is
    // invisibility, which is a different feature with different consent
    // questions. Refused, and named — not a generic validation error.
    const refused = await api(request, alice).put('/api/presence', { status: 'offline' });
    expect(refused.status()).toBe(400);
    expect((await refused.json()).message).toContain('derived');

    const nonsense = await api(request, alice).put('/api/presence', { status: 'in a meeting' });
    expect(nonsense.status()).toBe(400);

    // Refusals changed nothing.
    expect((await seen(request, alice))[alice.address]).toBe('away');
  });

  test('a block hides presence in both directions', async ({ request }) => {
    const alice = await signIn(request, 'presence-block-alice');
    const bob = await signIn(request, 'presence-block-bob');
    await dm(request, alice, bob);
    for (const who of [alice, bob]) {
      await api(request, who).put('/api/presence', { status: 'online' });
    }

    expect((await seen(request, alice))[bob.address]).toBe('online');

    await api(request, alice).post('/api/users/block', { address: bob.address });

    // Both ways. A one-directional filter would let the blocked party watch
    // the blocker come and go, and would answer "did they block me?" by the
    // absence of a dot.
    expect((await seen(request, alice))[bob.address]).toBeUndefined();
    expect((await seen(request, bob))[alice.address]).toBeUndefined();
  });

  test('an open event stream is presence on its own, and closing it ends it', async ({
    request,
  }) => {
    const alice = await signIn(request, 'presence-stream-alice');
    const bob = await signIn(request, 'presence-stream-bob');
    await dm(request, alice, bob);

    // Alice never declares anything. Holding the stream open is the claim.
    const { ticket } = await json(await api(request, alice).post('/api/events/ticket'));
    const abort = new AbortController();
    const stream = await fetch(`${BASE}/api/events?ticket=${encodeURIComponent(ticket)}`, {
      signal: abort.signal,
    });
    expect(stream.status).toBe(200);

    // Read one chunk before asserting. The response headers arrive as soon as
    // the handler returns, but reading proves the stream is genuinely up
    // rather than merely accepted — and the server's first frame is the
    // `retry:` hint, which is always there.
    const reader = stream.body.getReader();
    await reader.read();

    await expect
      .poll(async () => (await seen(request, bob))[alice.address], {
        message: 'opening a stream should have made Alice present',
      })
      .toBe('online');

    abort.abort();

    // And letting go is how you leave. Polled rather than asserted once: the
    // connection task notices the dropped peer on its own schedule, and a bare
    // assertion here would be a flake generator.
    await expect
      .poll(async () => (await seen(request, bob))[alice.address], {
        message: 'closing the last stream should have taken Alice offline',
        timeout: 15_000,
      })
      .toBeUndefined();
  });
});
