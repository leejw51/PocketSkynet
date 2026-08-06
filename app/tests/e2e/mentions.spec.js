const { test, expect } = require('@playwright/test');
const { signIn, api, hash, json, dm, post, channel, join } = require('./helpers');

test.describe('mentions', () => {
  test('an @name in plaintext reaches the inbox and clears when the room is read', async ({
    request,
  }) => {
    const alice = await signIn(request, 'mn-inbox-alice');
    const bob = await signIn(request, 'mn-inbox-bob');
    const room = await dm(request, alice, bob);

    const sent = await post(request, alice, room, `morning @${bob.username}, can you look at this?`);

    const inbox = await json(await api(request, bob).get('/api/mentions'));
    const entry = inbox.find((m) => m.message.id === sent.id);
    expect(entry, JSON.stringify(inbox)).toBeDefined();
    expect(entry.roomId).toBe(room);
    expect(entry.isUnread).toBe(true);
    expect(entry.message.senderAddress).toBe(alice.address);
    expect(entry.roomKind).toBe('dm');

    // Alice named Bob, not herself.
    const hers = await json(await api(request, alice).get('/api/mentions'));
    expect(hers.find((m) => m.message.id === sent.id)).toBeUndefined();

    // The room list carries the badge, separately from unreadCount.
    let rooms = await json(await api(request, bob).get('/api/rooms'));
    let row = rooms.find((r) => r.id === room);
    expect(row.mentionCount).toBe(1);
    expect(row.unreadCount).toBeGreaterThanOrEqual(1);

    // Reading the room is what clears it — there is no second pointer.
    await api(request, bob).post(`/api/rooms/${room}/read`, { lastReadSerial: sent.msgSerial });

    const after = await json(await api(request, bob).get('/api/mentions'));
    expect(after.find((m) => m.message.id === sent.id).isUnread).toBe(false);
    rooms = await json(await api(request, bob).get('/api/rooms'));
    expect(rooms.find((r) => r.id === room).mentionCount).toBe(0);
  });

  test('punctuation and email addresses do not manufacture mentions', async ({ request }) => {
    const alice = await signIn(request, 'mn-punct-alice');
    const bob = await signIn(request, 'mn-punct-bob');
    const room = await dm(request, alice, bob);

    const before = (await json(await api(request, bob).get('/api/mentions'))).length;

    // An email is not a mention of its domain, and a bare @ names nobody.
    await post(request, alice, room, 'write to bob@example.com, cost @ 5 dollars');
    expect((await json(await api(request, bob).get('/api/mentions'))).length).toBe(before);

    // But a trailing full stop is sentence punctuation, not part of the name.
    const named = await post(request, alice, room, `thanks @${bob.username}.`);
    const inbox = await json(await api(request, bob).get('/api/mentions'));
    expect(inbox.find((m) => m.message.id === named.id)).toBeDefined();
  });

  test('an encrypted message mentions through the declared list only', async ({ request }) => {
    const alice = await signIn(request, 'mn-enc-alice');
    const bob = await signIn(request, 'mn-enc-bob');
    const carol = await signIn(request, 'mn-enc-carol');
    const room = await dm(request, alice, bob);

    const sent = await post(request, alice, room, '9tYbG0mQ2sZ1Xh==', {
      // Carol is not in this room. Naming her must do nothing, or the mention
      // would leak the room's existence to somebody who was never in it.
      mentions: [bob.address, carol.address],
    });

    const bobs = await json(await api(request, bob).get('/api/mentions'));
    expect(bobs.find((m) => m.message.id === sent.id)).toBeDefined();

    const carols = await json(await api(request, carol).get('/api/mentions'));
    expect(carols.find((m) => m.message.id === sent.id)).toBeUndefined();
  });

  test('a mention list is validated, not trusted', async ({ request }) => {
    const alice = await signIn(request, 'mn-valid-alice');
    const bob = await signIn(request, 'mn-valid-bob');
    const room = await dm(request, alice, bob);

    const malformed = await api(request, alice).post(`/api/rooms/${room}/messages`, {
      content: 'hi',
      msgHash: hash('a'),
      mentions: ['not-an-address'],
    });
    expect(malformed.status()).toBe(400);

    const tooMany = await api(request, alice).post(`/api/rooms/${room}/messages`, {
      content: 'hi',
      msgHash: hash('a'),
      mentions: Array.from({ length: 40 }, () => bob.address),
    });
    expect(tooMany.status()).toBe(400);
  });

  test('deleting the message removes the mention', async ({ request }) => {
    const alice = await signIn(request, 'mn-del-alice');
    const bob = await signIn(request, 'mn-del-bob');
    const room = await dm(request, alice, bob);
    const sent = await post(request, alice, room, `@${bob.username} look at this`);

    expect(
      (await json(await api(request, bob).get('/api/mentions'))).find(
        (m) => m.message.id === sent.id,
      ),
    ).toBeDefined();

    await api(request, alice).delete(`/api/messages/${sent.id}`);

    expect(
      (await json(await api(request, bob).get('/api/mentions'))).find(
        (m) => m.message.id === sent.id,
      ),
      'the content is gone, so the pointer into it must be too',
    ).toBeUndefined();
  });

  test('editing a mention away removes it, and editing one in adds it', async ({ request }) => {
    const alice = await signIn(request, 'mn-edit-alice');
    const bob = await signIn(request, 'mn-edit-bob');
    const room = await dm(request, alice, bob);

    const sent = await post(request, alice, room, `@${bob.username} one more thing`);
    const inboxHas = async () =>
      (await json(await api(request, bob).get('/api/mentions'))).some(
        (m) => m.message.id === sent.id,
      );
    expect(await inboxHas()).toBe(true);

    await api(request, alice).patch(`/api/messages/${sent.id}`, {
      content: 'never mind, sorted it',
      msgHash: hash('e'),
    });
    expect(await inboxHas()).toBe(false);

    // And back again — the edit path replaces the set rather than only adding.
    await api(request, alice).patch(`/api/messages/${sent.id}`, {
      content: `actually @${bob.username} could you check`,
      msgHash: hash('f'),
    });
    expect(await inboxHas()).toBe(true);
  });

  test('a mention in a thread reply still reaches the inbox', async ({ request }) => {
    const alice = await signIn(request, 'mn-thread-alice');
    const bob = await signIn(request, 'mn-thread-bob');
    const room = await dm(request, alice, bob);
    const root = await post(request, alice, room, 'starting a thread');
    const reply = await post(request, alice, room, `over to you @${bob.username}`, {
      parentMessageId: root.id,
    });

    const inbox = await json(await api(request, bob).get('/api/mentions'));
    const entry = inbox.find((m) => m.message.id === reply.id);
    expect(entry).toBeDefined();
    expect(entry.message.parentMessageId).toBe(root.id);
  });

  test('leaving a room takes its mentions out of the inbox', async ({ request }) => {
    const alice = await signIn(request, 'mn-leave-alice');
    const bob = await signIn(request, 'mn-leave-bob');
    const room = (await json(await api(request, alice).post('/api/rooms', { name: 'Temporary' })))
      .id;
    await api(request, alice).post(`/api/rooms/${room}/invite`, { userAddress: bob.address });
    await api(request, bob).post(`/api/invitations/${room}/accept`);

    const sent = await post(request, alice, room, `welcome @${bob.username}`);
    expect(
      (await json(await api(request, bob).get('/api/mentions'))).some(
        (m) => m.message.id === sent.id,
      ),
    ).toBe(true);

    await api(request, bob).post(`/api/rooms/${room}/leave`);

    expect(
      (await json(await api(request, bob).get('/api/mentions'))).some(
        (m) => m.message.id === sent.id,
      ),
      'an inbox entry that 403s when opened is worse than no entry',
    ).toBe(false);
  });
});
