const { test, expect, devices } = require('@playwright/test');
const { signIn, api, json, channel, join, post, walletFor, BASE } = require('./helpers');

// Phone metrics, because the scroll rules are the one part of this client whose
// correctness depends on the size of the screen. A flat pixel threshold that is
// a quarter of a laptop's message pane is a sliver of a phone's, where a single
// bubble can be taller than the whole allowance — so a thumb-scroll landing one
// message short of the end read as "gone off to read history" and the stream
// stopped following. That regression is invisible at desktop size.
test.use({ ...devices['iPhone 13'] });

async function signInOnPhone(page, label) {
  await page.goto(BASE);
  const skip = page.getByRole('button', { name: /^skip$/i });
  if (await skip.count()) await skip.click().catch(() => {});
  await page.getByRole('button', { name: 'English', exact: true }).click();
  await page.getByRole('tab', { name: 'Private key' }).click();
  await page.getByRole('textbox', { name: 'Username' }).fill(label);
  await page.getByRole('textbox', { name: 'Private key' }).fill(walletFor(label).privateKey);
  await page.locator('button:text-is("Sign in")').click();
}

test.describe('on a phone', () => {
  test('sending scrolls to the newest message', async ({ page, request }) => {
    const alice = await signIn(request, 'ph-send-alice');
    const bob = await signIn(request, 'ph-send-bob');
    const room = await channel(request, alice, 'Pocket');
    await join(request, alice, bob, room);
    for (let i = 0; i < 25; i++) await post(request, bob, room, `line ${i}`);

    const errors = [];
    page.on('pageerror', (e) => errors.push(String(e)));

    await signInOnPhone(page, 'ph-send-alice');
    const opener = page.getByRole('option', { name: /Pocket/ });
    await opener.waitFor({ timeout: 25_000 });
    await opener.click();

    const stream = page.getByRole('log');
    await stream.waitFor({ timeout: 25_000 });
    const distanceFromBottom = () =>
      stream.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    const slack = async () =>
      stream.evaluate((el) => Math.max(120, el.clientHeight * 0.2));

    // Opening lands at the newest message.
    await expect.poll(distanceFromBottom, { timeout: 15_000 }).toBeLessThan(await slack());

    // Sending brings the view with it — the case that was reported broken.
    const box = page.getByRole('textbox', { name: /^Message / });
    await box.fill('sent from a phone');
    await box.press('Enter');
    await expect.poll(distanceFromBottom, { timeout: 20_000 }).toBeLessThan(await slack());
    await expect(stream.getByText('sent from a phone')).toBeVisible();
    // Nothing to escape from, so no jump pill.
    await expect(page.locator('.fn-jump-latest')).toHaveCount(0);

    // And it really reached the server.
    await expect
      .poll(async () => {
        const listed = await json(await api(request, alice).get(`/api/rooms/${room}/messages`));
        return listed.map((m) => m.content);
      }, { timeout: 15_000 })
      .toContain('sent from a phone');

    expect(errors, errors.join(' | ')).toHaveLength(0);
  });

  test('stays at the bottom when a media row grows after it loads', async ({ page, request }) => {
    // The reported bug, and the one every earlier test missed because they all
    // used fixed-height text. A message carrying a video or an image renders at
    // almost no height — no poster, no intrinsic dimensions — so the settle
    // runs against a row that is about to become several hundred pixels taller.
    // Content growth fires no scroll event, so nothing re-settled and the
    // newest message ended up off the bottom of the screen.
    const alice = await signIn(request, 'ph-media-alice');
    const bob = await signIn(request, 'ph-media-bob');
    const room = await channel(request, alice, 'Gallery');
    await join(request, alice, bob, room);
    for (let i = 0; i < 20; i++) await post(request, bob, room, `message ${i}`);

    // Hold images back so they land *after* the settle — what a large video
    // poster does on a phone, made deterministic.
    await page.route('**/*.{png,jpg,jpeg,webp,gif}', async (route) => {
      await new Promise((r) => setTimeout(r, 1200));
      await route.continue();
    });

    await signInOnPhone(page, 'ph-media-alice');
    const opener = page.getByRole('option', { name: /Gallery/ });
    await opener.waitFor({ timeout: 25_000 });
    await opener.click();

    const stream = page.getByRole('log');
    await stream.waitFor({ timeout: 25_000 });
    const geo = () =>
      stream.evaluate((el) => ({
        d: el.scrollHeight - el.scrollTop - el.clientHeight,
        h: el.scrollHeight,
      }));
    const slack = await stream.evaluate((el) => Math.max(120, el.clientHeight * 0.2));
    await expect.poll(async () => (await geo()).d, { timeout: 20_000 }).toBeLessThan(slack);

    const before = (await geo()).h;
    await post(request, bob, room, 'https://placehold.co/600x900.png');

    // The row must actually grow, or this test proves nothing.
    await expect
      .poll(async () => (await geo()).h, { timeout: 25_000 })
      .toBeGreaterThan(before + 200);
    // ...and the view must still be at the newest message afterwards.
    await expect.poll(async () => (await geo()).d, { timeout: 15_000 }).toBeLessThan(slack);
    await expect(page.locator('.fn-jump-latest')).toHaveCount(0);
  });

  test('a message arriving while pinned still follows on a small screen', async ({
    page,
    request,
  }) => {
    const alice = await signIn(request, 'ph-recv-alice');
    const bob = await signIn(request, 'ph-recv-bob');
    const room = await channel(request, alice, 'Handset');
    await join(request, alice, bob, room);
    for (let i = 0; i < 25; i++) await post(request, bob, room, `line ${i}`);

    await signInOnPhone(page, 'ph-recv-alice');
    const opener = page.getByRole('option', { name: /Handset/ });
    await opener.waitFor({ timeout: 25_000 });
    await opener.click();

    const stream = page.getByRole('log');
    await stream.waitFor({ timeout: 25_000 });
    const distanceFromBottom = () =>
      stream.evaluate((el) => el.scrollHeight - el.scrollTop - el.clientHeight);
    const slack = await stream.evaluate((el) => Math.max(120, el.clientHeight * 0.2));
    await expect.poll(distanceFromBottom, { timeout: 15_000 }).toBeLessThan(slack);

    await post(request, bob, room, 'landed on the phone');
    await expect(stream.getByText('landed on the phone')).toBeVisible({ timeout: 20_000 });
    await expect.poll(distanceFromBottom, { timeout: 15_000 }).toBeLessThan(slack);
    await expect(page.locator('.fn-jump-latest')).toHaveCount(0);
  });
});
