const { test, expect } = require('@playwright/test');
const { BASE, signIn, channel, dm, post, walletFor } = require('./helpers');

// Does adding a message *add* a message, or rebuild the whole list?
//
// Nothing else in this suite can tell the difference. Every assertion about
// content passes either way: the text is correct whether the row was kept or
// torn down and built again from the same data. What the user sees is not the
// same, though — a rebuilt row loses its images (they refetch), re-decrypts its
// ciphertext, and has no height for a frame, which is the flicker, and is also
// what strands the newest message off-screen when the scroll position is
// clamped against a list that momentarily collapsed.
//
// So this file asserts on node *identity*: mark every row, change the list,
// and require the marked nodes to still be there. It is the only way the
// defect is visible from outside, and it went unnoticed for exactly that
// reason — the rule that governs it (every child of a list needs a key, or
// none of the keys are used) is invisible in the markup and silent when broken.

/// Tag every row so we can tell a survivor from a replacement.
const mark = (page, selector) =>
  page.evaluate((sel) => {
    const rows = document.querySelectorAll(sel);
    rows.forEach((r) => {
      r.dataset.marked = '1';
    });
    return rows.length;
  }, selector);

/// How many of the tagged nodes are still in the document, and how many are new.
const survivors = (page, selector) =>
  page.evaluate((sel) => {
    let kept = 0;
    let fresh = 0;
    document.querySelectorAll(sel).forEach((r) => (r.dataset.marked ? kept++ : fresh++));
    return { kept, fresh };
  }, selector);

async function signInAs(page, label) {
  await page.goto(BASE);
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

test.describe('re-rendering', () => {
  test('a new message adds one row and keeps the rest', async ({ page, request }) => {
    const me = await signIn(request, 'rr-arrive');
    const room = await channel(request, me, 'Rerender arrivals');
    // Enough rows that positional matching and keyed matching cannot agree by
    // accident: with one or two messages, "rebuilt everything" and "added one"
    // are the same list.
    for (let i = 0; i < 12; i++) await post(request, me, room, `line ${i}`);

    await signInAs(page, 'rr-arrive');
    await page.getByRole('option', { name: /Rerender arrivals/ }).click();
    const rows = page.locator('.fn-stream article');
    await expect(rows).toHaveCount(12, { timeout: 20_000 });

    expect(await mark(page, '.fn-stream article')).toBe(12);
    await post(request, me, room, 'the new one');
    await expect(rows).toHaveCount(13, { timeout: 20_000 });

    // The whole point: twelve survivors, one newcomer. A rebuild reports
    // `{ kept: 0, fresh: 13 }`, which is what this used to do.
    expect(await survivors(page, '.fn-stream article')).toEqual({ kept: 12, fresh: 1 });
  });

  test('sending from the composer keeps the rows already on screen', async ({ page, request }) => {
    const me = await signIn(request, 'rr-send');
    const room = await channel(request, me, 'Rerender sends');
    for (let i = 0; i < 12; i++) await post(request, me, room, `line ${i}`);

    await signInAs(page, 'rr-send');
    await page.getByRole('option', { name: /Rerender sends/ }).click();
    const rows = page.locator('.fn-stream article');
    await expect(rows).toHaveCount(12, { timeout: 20_000 });

    expect(await mark(page, '.fn-stream article')).toBe(12);

    // The send path differs from the arrival path: a pending bubble appears
    // first and is replaced by the acknowledged message. Both are list edits,
    // and both used to renumber every sibling.
    const box = page.getByRole('textbox', { name: /^Message / });
    await box.fill('typed and sent');
    await box.press('Enter');
    await expect(rows).toHaveCount(13, { timeout: 20_000 });
    await expect
      .poll(async () => (await survivors(page, '.fn-stream article')).kept, { timeout: 15_000 })
      .toBe(12);
  });

  test('a room list edit keeps the room rows', async ({ page, request }) => {
    // The sidebar had the same defect, from the same cause: the section
    // heading ("Channels" / "Direct messages") sat unkeyed beside the rows,
    // so opening a room rebuilt every row in the list.
    const me = await signIn(request, 'rr-sidebar');
    const peer = await signIn(request, 'rr-sidebar-peer');
    const rooms = [];
    for (let i = 0; i < 5; i++) rooms.push(await channel(request, me, `Sidebar ${i}`));
    // The heading is the unkeyed sibling, and it is only drawn when *both*
    // sections exist — so a list of channels alone cannot show this defect.
    // Without the DM this test passes against the broken client, which is
    // worse than not having it.
    await dm(request, me, peer);

    await signInAs(page, 'rr-sidebar');
    const options = page.locator('[role="option"]');
    await expect(options).toHaveCount(6, { timeout: 20_000 });
    expect(await mark(page, '[role="option"]')).toBe(6);

    // Reorder the list: the sidebar sorts by last activity, so a message in
    // the oldest room moves it to the top and every row below it shifts down.
    // A reorder is the edit positional matching cannot survive; merely opening
    // a room leaves every position where it was and looks fine either way.
    await post(request, me, rooms[0], 'bump');
    await expect(page.locator('[role="option"]').first()).toContainText('Sidebar 0', {
      timeout: 20_000,
    });
    await expect
      .poll(async () => (await survivors(page, '[role="option"]')).kept, { timeout: 15_000 })
      .toBe(6);
  });
});
