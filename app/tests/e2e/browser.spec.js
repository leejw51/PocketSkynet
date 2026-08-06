const { test, expect } = require('@playwright/test');
const { BASE, signIn, api, json, dm, post, channel, join, walletFor } = require('./helpers');

// The other specs drive the HTTP API. This one drives the actual WASM client in
// a real browser, because the wire shape changed underneath it: rooms grew
// `kind`, messages grew `parentMessageId` / `replyCount` / `lastReplyAt`, and
// the room list grew `mentionCount`. A client that deserialised strictly would
// break on any of those, and nothing in the Rust suite would notice — it does
// not run the client.

/// Sign in through the UI as a specific wallet.
///
/// The private-key tab rather than "create a wallet": the generate flow
/// deliberately refuses to continue until you have saved the phrase, and more
/// importantly a test needs to be *this* person, so that data set up over the
/// API shows up on screen.
async function signInAs(page, label) {
  await page.goto(BASE);
  // The boot animation covers the sign-in form until it finishes or is
  // skipped. It only appears on some layouts, so this is conditional rather
  // than unconditional — on a phone it is the difference between signing in
  // and timing out on an invisible button.
  const skip = page.getByRole('button', { name: /^skip$/i });
  if (await skip.count()) await skip.click().catch(() => {});
  await page.getByRole('button', { name: 'English', exact: true }).click();
  await page.getByRole('tab', { name: 'Private key' }).click();
  await page.getByRole('textbox', { name: 'Username' }).fill(label);
  await page.getByRole('textbox', { name: 'Private key' }).fill(walletFor(label).privateKey);
  await page.locator('button:text-is("Sign in")').click();
  await expect(page.getByRole('complementary', { name: 'Rooms' })).toBeVisible({
    timeout: 20_000,
  });
}

function watchForErrors(page) {
  const errors = [];
  page.on('console', (m) => {
    if (m.type() === 'error') errors.push(m.text());
  });
  page.on('pageerror', (e) => errors.push(String(e)));
  return errors;
}

test.describe('the web client', () => {
  test('signs in and reaches an empty room list without console errors', async ({ page }) => {
    const errors = watchForErrors(page);
    await signInAs(page, 'ui-fresh');

    await expect(page.getByRole('heading', { name: 'No rooms yet' })).toBeVisible();
    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('renders a channel whose messages carry the new thread fields', async ({
    page,
    request,
  }) => {
    // Set the room up over the API as the same wallet the browser will use,
    // then let the real client render it.
    const alice = await signIn(request, 'ui-threads');
    const room = await channel(request, alice, 'Release');
    const root = await post(request, alice, room, 'does the client still render me?');
    await post(request, alice, room, 'a threaded reply', { parentMessageId: root.id });

    // Confirm the server really is sending the new shape before blaming the UI.
    const listed = await json(await api(request, alice).get(`/api/rooms/${room}/messages`));
    expect(listed.find((m) => m.id === root.id).replyCount).toBe(1);

    const errors = watchForErrors(page);
    await signInAs(page, 'ui-threads');

    await page.getByRole('option', { name: /Release/ }).click();

    // Scoped to the message log. The room-list preview quotes the newest
    // message in the room whether or not it is a reply — deliberately, so a
    // room with thread-only activity does not look idle — so an unscoped
    // match would find the reply in the sidebar and prove nothing.
    const log = page.getByRole('log');
    await expect(log.getByText('does the client still render me?')).toBeVisible({
      timeout: 15_000,
    });

    // The reply is NOT in the channel — it is collapsed under its parent,
    // which is the entire point of a thread. `/sync` delivered it (it has to,
    // or an offline client would lose it) and the client holds it, which is
    // what makes opening the thread instant and offline.
    await expect(log.getByText('a threaded reply')).toHaveCount(0);

    // The parent says how much is under it, and opening it reveals exactly
    // that — from local state, with no request.
    const opener = page.getByRole('button', { name: /1 reply/ });
    await expect(opener).toBeVisible();
    await opener.click();
    await expect(log.getByText('a threaded reply')).toBeVisible();

    // And it closes again.
    await opener.click();
    await expect(log.getByText('a threaded reply')).toHaveCount(0);

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('renders a DM and its mention badge', async ({ page, request }) => {
    const alice = await signIn(request, 'ui-dm-alice');
    const bob = await signIn(request, 'ui-dm-bob');
    const room = await dm(request, bob, alice);
    await post(request, bob, room, `morning @${alice.username}, ready when you are`);

    const rooms = await json(await api(request, alice).get('/api/rooms'));
    const row = rooms.find((r) => r.id === room);
    expect(row.kind).toBe('dm');
    expect(row.mentionCount).toBe(1);

    const errors = watchForErrors(page);
    await signInAs(page, 'ui-dm-alice');

    // A DM is a room, so the client lists and opens it through the same
    // machinery as a channel — the point of building DMs on the room
    // primitive rather than beside it. What differs is the title: the server
    // stores a placeholder ("Direct message") because the column is NOT NULL,
    // and the client replaces it with whoever else is in the room. So the row
    // is named after Bob, and the placeholder must be nowhere on screen.
    await expect(page.getByRole('heading', { name: 'No rooms yet' })).toHaveCount(0);
    await expect(page.getByText('Direct message', { exact: true })).toHaveCount(0);
    await page.getByRole('option', { name: new RegExp(bob.username) }).click();
    await expect(page.getByRole('log').getByText(/ready when you are/)).toBeVisible({
      timeout: 15_000,
    });

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('starts a direct message from the picker', async ({ page, request }) => {
    // The flow a person actually uses: press "New message", search, press
    // Message, land in the conversation. Everything else in this file sets
    // rooms up over the API, which proves rendering but not reachability.
    const alice = await signIn(request, 'ui-pick-alice');
    const bob = await signIn(request, 'ui-pick-bob');

    const errors = watchForErrors(page);
    await signInAs(page, 'ui-pick-alice');
    await expect(page.getByRole('heading', { name: 'No rooms yet' })).toBeVisible();

    await page.getByRole('button', { name: 'New message' }).click();
    await page.getByRole('searchbox', { name: 'New message' }).fill(bob.username);
    await page.getByRole('button', { name: 'Message', exact: true }).click();

    // Landed in the conversation, titled after Bob rather than after nothing.
    const composer = page.getByRole('textbox', { name: new RegExp(`^Message ${bob.username}`) });
    await expect(composer).toBeVisible({ timeout: 15_000 });

    // And it is the same room the API would have opened — idempotence, seen
    // from the outside.
    const rooms = await json(await api(request, bob).get('/api/rooms'));
    expect(rooms).toHaveLength(1);
    expect(rooms[0].kind).toBe('dm');
    expect(rooms[0].members.map((m) => m.userAddress).sort()).toEqual(
      [alice.address, bob.address].sort(),
    );

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('replies in a thread from the composer, and highlights a mention', async ({
    page,
    request,
  }) => {
    const alice = await signIn(request, 'ui-reply-alice');
    const bob = await signIn(request, 'ui-reply-bob');
    const room = await channel(request, alice, 'Standup');
    await join(request, alice, bob, room);
    const root = await post(request, bob, room, 'who is picking up the migration?');

    const errors = watchForErrors(page);
    await signInAs(page, 'ui-reply-alice');
    await page.getByRole('option', { name: /Standup/ }).click();
    await expect(page.getByRole('log').getByText(/picking up the migration/)).toBeVisible({
      timeout: 15_000,
    });

    // Reply into the thread. The affordance lives on the message's tools rail.
    await page.getByRole('article').filter({ hasText: 'migration' })
      .getByRole('button', { name: 'Reply in thread' }).click();
    // The composer says where this is going — silently posting somewhere other
    // than the channel is the worst outcome this screen can produce.
    await expect(page.getByText(/Reply in thread ·/)).toBeVisible();

    const box = page.getByRole('textbox', { name: /^Message / });
    await box.fill('I can take it');
    await box.press('Enter');

    // It landed as a reply, not as a channel message.
    await expect
      .poll(async () => {
        const all = await json(
          await api(request, alice).get(`/api/rooms/${room}/messages?includeReplies=true`),
        );
        const mine = all.find((m) => m.content === 'I can take it');
        return mine?.parentMessageId ?? null;
      }, { timeout: 15_000 })
      .toBe(root.id);

    // The chip clears after sending: a sticky thread is how people post an
    // unrelated remark into somebody else's conversation.
    await expect(page.getByText(/Reply in thread ·/)).toHaveCount(0);

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('completes an @mention from the composer', async ({ page, request }) => {
    const alice = await signIn(request, 'ui-at-alice');
    const bob = await signIn(request, 'ui-at-bob');
    const room = await channel(request, alice, 'Design');
    await join(request, alice, bob, room);

    const errors = watchForErrors(page);
    await signInAs(page, 'ui-at-alice');
    await page.getByRole('option', { name: /Design/ }).click();

    const box = page.getByRole('textbox', { name: /^Message / });
    await box.fill('morning @ui-at-b');
    // The picker offers Bob and never the viewer — naming yourself is a no-op.
    const option = page.getByRole('option', { name: new RegExp(bob.username) });
    await expect(option).toBeVisible({ timeout: 10_000 });
    await expect(page.getByRole('option', { name: new RegExp(alice.username) })).toHaveCount(0);

    await box.press('Enter'); // Enter accepts the suggestion, it does not send.
    await expect(box).toHaveValue(`morning @${bob.username} `);
    await box.press('Enter'); // Now it sends.

    // It reached Bob's inbox as a real mention, not merely as text.
    await expect
      .poll(async () => {
        const inbox = await json(await api(request, bob).get('/api/mentions'));
        return inbox.map((m) => m.message.content);
      }, { timeout: 15_000 })
      .toContain(`morning @${bob.username}`);

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('opens the mentions inbox and jumps to the room', async ({ page, request }) => {
    const alice = await signIn(request, 'ui-inbox-alice');
    const bob = await signIn(request, 'ui-inbox-bob');
    const room = await channel(request, alice, 'Roadmap');
    await join(request, alice, bob, room);
    await post(request, bob, room, `can you review this @${alice.username}?`);

    const errors = watchForErrors(page);
    await signInAs(page, 'ui-inbox-alice');

    await page.getByRole('button', { name: 'Mentions', exact: true }).click();
    // Scoped to the dialog: the same text is in the room-list preview behind
    // it, and an unscoped match would find that instead and prove nothing.
    const entry = page.getByRole('dialog').getByText(/can you review this/);
    await expect(entry).toBeVisible({ timeout: 15_000 });

    // Tapping it takes you to the room the mention is in.
    await entry.click();
    await expect(page.getByRole('textbox', { name: /^Message Roadmap/ })).toBeVisible({
      timeout: 15_000,
    });
    // And the mention is highlighted where it was written.
    await expect(page.getByRole('log').locator('.fn-mention')).toBeVisible();

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('shows the admin console only to an administrator', async ({ page, request }) => {
    // `boss` is the one wallet in this server's VITE_FRUITNATION_ADMIN, so
    // this exercises the real configuration path rather than a test hook.
    const alice = await signIn(request, 'ui-admin-alice');
    await channel(request, alice, 'Finance');

    const errors = watchForErrors(page);

    // An ordinary member is not offered it.
    await signInAs(page, 'ui-admin-alice');
    await expect(page.getByRole('button', { name: 'Server admin' })).toHaveCount(0);
    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('an administrator gets the console', async ({ page, request }) => {
    const alice = await signIn(request, 'ui-admin2-alice');
    // A name unique across the whole suite: the console lists *every* room on
    // the server, so two tests naming a room the same way makes this assertion
    // ambiguous — which is a property of the console being global, not a bug.
    await channel(request, alice, 'Procurement');

    const errors = watchForErrors(page);
    await signInAs(page, 'boss');
    await page.getByRole('button', { name: 'Server admin' }).click();

    // People and rooms, with the configured admin list echoed back so a typo
    // in VITE_FRUITNATION_ADMIN is visible somewhere.
    await expect(page.getByText(/people ·/)).toBeVisible({ timeout: 15_000 });
    await expect(page.getByRole('tab', { name: 'People' })).toBeVisible();
    await expect(page.getByText(/Administrators come from VITE_FRUITNATION_ADMIN/)).toBeVisible();

    await page.getByRole('tab', { name: 'Rooms' }).click();
    // Scoped to the dialog: `boss` is not a member of Finance and so cannot
    // see it in the sidebar — but the *tab* heading is also called "Rooms",
    // and an unscoped text match finds that too.
    await expect(page.getByRole('dialog').getByText('Procurement')).toBeVisible();

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('follows new messages when pinned, and holds still when reading back', async ({
    page,
    request,
  }) => {
    // The two halves of WhatsApp-style scrolling, which are one feature: it
    // follows the newest message *because* it can tell you are at the bottom.
    // Without the second half, "always scroll down" drags you out of the
    // history you are reading — worse than not scrolling at all.
    const alice = await signIn(request, 'ui-scroll-alice');
    const bob = await signIn(request, 'ui-scroll-bob');
    const room = await channel(request, alice, 'Backlog');
    await join(request, alice, bob, room);
    // Enough to overflow the viewport several times over.
    for (let i = 0; i < 40; i++) {
      await post(request, bob, room, `backlog line ${i}`);
    }

    const errors = watchForErrors(page);
    await signInAs(page, 'ui-scroll-alice');
    await page.getByRole('option', { name: /Backlog/ }).click();

    const stream = page.getByRole('log');
    await expect(stream.getByText('backlog line 39')).toBeVisible({ timeout: 15_000 });

    const distanceFromBottom = () =>
      stream.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);

    // Opening a room lands at the bottom. Polled rather than asserted once:
    // `toBeVisible` above does NOT mean "in the viewport" — Playwright counts
    // any element with a box as visible — so it can pass while the stream is
    // still at the top, which is exactly how the first version of this test
    // failed against correct behaviour.
    await expect.poll(distanceFromBottom, { timeout: 10_000 }).toBeLessThan(120);

    // A new message arrives while pinned: it follows.
    await post(request, bob, room, 'arrived while pinned');
    await expect(stream.getByText('arrived while pinned')).toBeVisible({ timeout: 15_000 });
    await expect.poll(distanceFromBottom, { timeout: 10_000 }).toBeLessThan(120);
    // Nothing to escape from, so no pill. Matched by class, not by name: the
    // room-list toolbar has a "New message" button (the DM picker) that a
    // loose /new message/i also matches — which made the pill look present
    // when it was not.
    await expect(page.locator('.fn-jump-latest')).toHaveCount(0);

    // Scroll up with a real wheel gesture, not `scrollTo`.
    //
    // The distinction is the feature, not pedantry: the client only treats a
    // scroll as "the reader moved" when a gesture is behind it, because the
    // browser also fires `scroll` when content resizes under the viewport —
    // and taking that for a deliberate scroll is what stranded the newest
    // message off-screen. A synthetic `scrollTo` has no gesture, so it would
    // test a path no person can reach.
    await stream.hover();
    await page.mouse.wheel(0, -4000);
    await expect.poll(distanceFromBottom, { timeout: 5_000 }).toBeGreaterThan(400);
    const before = await stream.evaluate((el) => el.scrollTop);

    await post(request, bob, room, 'arrived while reading back');
    // The pill appears...
    const pill = page.locator('.fn-jump-latest');
    await expect(pill).toBeVisible({ timeout: 15_000 });
    // ...and the reader was NOT dragged away. Compared with slack rather than
    // exactly: appending content below makes the browser's own scroll
    // anchoring nudge scrollTop by a few pixels to hold the anchored element
    // still, which is the mechanism working, not the reader being moved.
    const after = await stream.evaluate((el) => el.scrollTop);
    expect(Math.abs(after - before)).toBeLessThan(50);
    expect(await distanceFromBottom()).toBeGreaterThan(400);

    // Pressing it returns to the newest message and clears the pill.
    await pill.click();
    await expect.poll(distanceFromBottom, { timeout: 10_000 }).toBeLessThan(120);
    await expect(stream.getByText('arrived while reading back')).toBeVisible();
    await expect(pill).toHaveCount(0);

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('sending your own message scrolls you to it, even from up the history', async ({
    page,
    request,
  }) => {
    // The case the first version missed entirely: your own message appears as
    // a *pending* bubble, which lives in `store.pending` and not in the
    // message map — so a settle keyed on the message count never fired for
    // the single most obvious interaction there is.
    const alice = await signIn(request, 'ui-ownsend-alice');
    const bob = await signIn(request, 'ui-ownsend-bob');
    const room = await channel(request, alice, 'Ownsend');
    await join(request, alice, bob, room);
    for (let i = 0; i < 40; i++) await post(request, bob, room, `history ${i}`);

    const errors = watchForErrors(page);
    await signInAs(page, 'ui-ownsend-alice');
    await page.getByRole('option', { name: /Ownsend/ }).click();

    const stream = page.getByRole('log');
    // Wait for the history to actually be on screen before measuring it.
    // `getByRole('log')` resolves as soon as the empty stream exists, and an
    // empty stream is not scrollable — so "distance from the bottom is under
    // 120px" passed trivially at zero, the scroll-to-top below did nothing,
    // and the test then failed further down for a reason that had nothing to
    // do with what it was checking. It only surfaced when the whole file ran,
    // which is the tell: the assertion was racing the first paint.
    await expect(page.locator('.fn-stream article')).toHaveCount(40, { timeout: 30_000 });
    const distanceFromBottom = () =>
      stream.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    await expect.poll(distanceFromBottom, { timeout: 10_000 }).toBeLessThan(120);

    // Scroll well up the history first — sending must override that, because
    // somebody who scrolled up and then typed wants to see what they said.
    await stream.evaluate((el) => el.scrollTo({ top: 0, behavior: 'instant' }));
    await expect.poll(distanceFromBottom, { timeout: 5_000 }).toBeGreaterThan(400);

    const box = page.getByRole('textbox', { name: /^Message / });
    await box.fill('my own message');
    await box.press('Enter');

    await expect.poll(distanceFromBottom, { timeout: 15_000 }).toBeLessThan(120);
    await expect(stream.getByText('my own message')).toBeVisible();
    // No jump pill: it followed, so there is nothing to escape from.
    await expect(page.locator('.fn-jump-latest')).toHaveCount(0);

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('holds the newest message through a cold open, threads and growing media', async ({
    page,
    request,
  }) => {
    // The reported room, reproduced: opened cold (straight to its URL, so the
    // component mounts on a spinner and the stream element does not exist
    // yet), carrying threads and a row that grows long after it first paints.
    //
    // Each of those broke the settle in a different way, and each was invisible
    // to a test that opened a warm room full of fixed-height text:
    //
    //  * cold open — the media listeners were attached with `()` deps, so they
    //    ran once against a DOM with no `.fn-stream` in it and never retried;
    //  * threads — replies were counted as arrivals even while collapsed, so
    //    the pill offered "1 new message" for a row nowhere in the channel;
    //  * media — a row renders before its poster decodes, then grows, and
    //    content growth fires no scroll event.
    const alice = await signIn(request, 'ui-real-alice');
    const bob = await signIn(request, 'ui-real-bob');
    const room = await channel(request, alice, 'Realistic');
    await join(request, alice, bob, room);
    for (let i = 0; i < 15; i++) await post(request, bob, room, `message ${i}`);
    const root = await post(request, bob, room, 'thread root');
    await post(request, bob, room, 'reply one', { parentMessageId: root.id });
    await post(request, bob, room, 'reply two', { parentMessageId: root.id });
    for (let i = 15; i < 25; i++) await post(request, bob, room, `message ${i}`);

    // Images land after the settle, as a large video poster does.
    await page.route('**/*.{png,jpg,jpeg,webp,gif}', async (route) => {
      await new Promise((r) => setTimeout(r, 1200));
      await route.continue();
    });

    const errors = watchForErrors(page);
    await signInAs(page, 'ui-real-alice');
    // Cold: navigate to the room URL rather than clicking it in a warm list.
    await page.goto(`${BASE}/rooms/${room}`);

    const stream = page.getByRole('log');
    await stream.waitFor({ timeout: 25_000 });
    const geo = () =>
      stream.evaluate((el) => ({
        d: el.scrollHeight - el.scrollTop - el.clientHeight,
        h: el.scrollHeight,
      }));
    const slack = async () =>
      stream.evaluate((el) => Math.max(120, el.clientHeight * 0.2));
    await expect.poll(async () => (await geo()).d, { timeout: 25_000 })
      .toBeLessThan(await slack());

    // A reply to a *collapsed* thread must not claim to be a new message.
    await post(request, bob, room, 'reply three', { parentMessageId: root.id });
    await expect(page.getByRole('button', { name: /3 replies/ })).toBeVisible({
      timeout: 20_000,
    });
    await expect(page.locator('.fn-jump-latest')).toHaveCount(0);

    // Expanding a thread must not fling the view to the end of the room — it
    // is a reading action, and the replies just revealed are the point.
    //
    // Asserted as "still far from the bottom" rather than "scrollTop did not
    // change": inserting rows *above* the viewport makes the browser's own
    // scroll anchoring shift scrollTop by exactly the inserted height, to hold
    // the visible content still. That is the mechanism working, and an
    // equality check on scrollTop measures it instead of the fling.
    await page.getByRole('button', { name: /3 replies/ }).click();
    await expect(stream.getByText('reply two')).toBeVisible();
    // Give the revealed rows' avatars time to load — that is what used to fire
    // the media listener and carry the reader to the bottom.
    await page.waitForTimeout(2500);
    expect(
      (await geo()).d,
      'expanding a thread must not scroll to the end of the room',
    ).toBeGreaterThan(await slack());

    // Back to following before the media check: the reader is deliberately
    // parked up the history by the assertion above.
    await stream.evaluate((el) => el.scrollTo({ top: el.scrollHeight, behavior: 'instant' }));
    await expect.poll(async () => (await geo()).d, { timeout: 10_000 })
      .toBeLessThan(await slack());

    // A media row that grows after painting still ends up fully in view.
    const grewFrom = (await geo()).h;
    await post(request, bob, room, 'https://placehold.co/600x900.png');
    await expect.poll(async () => (await geo()).h, { timeout: 25_000 })
      .toBeGreaterThan(grewFrom + 200);
    await expect.poll(async () => (await geo()).d, { timeout: 15_000 })
      .toBeLessThan(await slack());

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('a reflow that moves the scroll does not count as reading history', async ({
    page,
    request,
  }) => {
    // The exact mechanism behind "after adding a message it does not scroll",
    // reduced to its essentials and made deterministic.
    //
    // Rows are not their final height when they first paint, and re-rendering
    // one collapses it and springs it back. While it is collapsed the browser
    // clamps `scrollTop`; when the height returns, the view is left hundreds
    // of pixels adrift — and the `scroll` event that clamp fires is
    // indistinguishable, on its own, from somebody scrolling up to read. Taken
    // for the latter, every correction afterwards politely refused to run.
    //
    // Only a gesture may unpin. This simulates the reflow with no gesture at
    // all, which is precisely the case that used to strand the view.
    const alice = await signIn(request, 'ui-reflow-alice');
    const bob = await signIn(request, 'ui-reflow-bob');
    const room = await channel(request, alice, 'Reflow');
    await join(request, alice, bob, room);
    for (let i = 0; i < 25; i++) await post(request, bob, room, `message ${i}`);

    const errors = watchForErrors(page);
    await signInAs(page, 'ui-reflow-alice');
    await page.getByRole('option', { name: /Reflow/ }).click();

    const stream = page.getByRole('log');
    await stream.waitFor({ timeout: 25_000 });
    const dist = () =>
      stream.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    const slack = await stream.evaluate((el) => Math.max(120, el.clientHeight * 0.2));
    await expect.poll(dist, { timeout: 15_000 }).toBeLessThan(slack);

    // Collapse the content and bring it back — a re-render, no gesture.
    //
    // Detaching and re-attaching the rows, because that is what the framework
    // actually does and what the observer watches: the real trace from the
    // reported room showed `childList` mutations, height dropping 2128 → 1211
    // and returning, with the browser clamping scrollTop in between.
    await stream.evaluate(async (el) => {
      const rows = [...el.children];
      rows.forEach((r) => r.remove());
      void el.scrollHeight;
      await new Promise((r) => setTimeout(r, 120));
      rows.forEach((r) => el.appendChild(r));
      void el.scrollHeight;
    });

    // The observer must put it back, because no gesture unpinned it.
    await expect.poll(dist, { timeout: 15_000 }).toBeLessThan(slack);

    // And a message sent afterwards still follows.
    const box = page.getByRole('textbox', { name: /^Message / });
    await box.fill('after the reflow');
    await box.press('Enter');
    await expect(stream.getByText('after the reflow')).toBeVisible({ timeout: 15_000 });
    await expect.poll(dist, { timeout: 15_000 }).toBeLessThan(slack);
    await expect(page.locator('.fn-jump-latest')).toHaveCount(0);

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('reacts to a message inside a thread', async ({ page, request }) => {
    const alice = await signIn(request, 'ui-react-alice');
    const bob = await signIn(request, 'ui-react-bob');
    const room = await channel(request, alice, 'Reactions');
    await join(request, alice, bob, room);
    const root = await post(request, bob, room, 'root of the thread');
    const reply = await post(request, bob, room, 'the reply to react to', {
      parentMessageId: root.id,
    });

    const errors = watchForErrors(page);
    await signInAs(page, 'ui-react-alice');
    await page.getByRole('option', { name: /Reactions/ }).click();
    await page.getByRole('button', { name: /1 reply/ }).click();

    const replyRow = page.getByRole('article').filter({ hasText: 'the reply to react to' });
    await expect(replyRow).toBeVisible({ timeout: 15_000 });

    // The react button lives on the row's own tools rail, inside the thread.
    await replyRow.getByRole('button', { name: /React to/ }).click();
    // The picker opens at the message, not over the composer.
    const picker = page.getByRole('dialog').or(page.locator('.fn-picker')).first();
    await expect(picker).toBeVisible({ timeout: 10_000 });
    await picker.getByRole('button').first().click();

    // It reached the server, attached to the REPLY and not to the root.
    await expect
      .poll(async () => {
        const agg = await json(await api(request, alice).get(`/api/messages/${reply.id}/emoticons`));
        return Array.isArray(agg) ? agg.length : 0;
      }, { timeout: 15_000 })
      .toBeGreaterThan(0);
    const rootAgg = await json(
      await api(request, alice).get(`/api/messages/${root.id}/emoticons`),
    );
    expect(rootAgg, 'the root must not have picked up the reply\'s reaction').toHaveLength(0);

    // And it renders back on the reply.
    await expect(replyRow.locator('.fn-reactions')).toBeVisible({ timeout: 15_000 });

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('sending a message from the browser still works', async ({ page, request }) => {
    const alice = await signIn(request, 'ui-send');
    const room = await channel(request, alice, 'Compose');

    const errors = watchForErrors(page);
    await signInAs(page, 'ui-send');
    await page.getByRole('option', { name: /Compose/ }).click();

    // The composer is labelled "Message <room name>".
    const box = page.getByRole('textbox', { name: /^Message / });
    await box.fill('typed in a real browser');
    await box.press('Enter');

    await expect(page.getByRole('log').getByText('typed in a real browser')).toBeVisible({
      timeout: 15_000,
    });

    // And it really reached the server, not just the optimistic local echo.
    await expect
      .poll(
        async () => {
          const listed = await json(await api(request, alice).get(`/api/rooms/${room}/messages`));
          return listed.map((m) => m.content);
        },
        { timeout: 15_000 },
      )
      .toContain('typed in a real browser');

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });
});
