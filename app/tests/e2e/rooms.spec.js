// The three built-in rooms, driven through a real browser.
//
// Everything the server enforces about these rooms is already proved by
// `server/tests/static_rooms.rs`. What no test had ever exercised is the half
// where the confirmed bugs actually lived: the WASM client. This suite signs
// in a real Chromium, reads the sidebar the user sees, and drives My Note and
// My Jarvis by clicking and typing — because "the reply never appeared" and
// "the menu still shows Delete" are statements about the rendered DOM, and the
// DOM is the only place they can be checked.
//
// The Jarvis test is the important one. Its regression guard is not "a reply
// appeared" but "the reply answered the question just typed": the bug was a
// stale state snapshot, so the model was asked the *previous* turn (or nothing)
// while a reply still came back. Only inspecting the intercepted request body
// catches that, so that assertion is the one that must never be weakened.
//
// Ordinary channels are created through the API (the same pattern
// rerender.spec.js uses) as the very wallet the browser then signs in as, so
// they are in the sidebar on first paint. That keeps these tests about the
// built-in rooms rather than about whichever create-room control the build
// happens to ship.

const { test, expect } = require("@playwright/test");
const { BASE, signIn, channel, walletFor } = require("./helpers");

// A fresh label per run, so a rerun never signs in as a user another run
// already gave rooms — the sidebar counts have to be facts, not leftovers.
let counter = 0;
const freshLabel = (tag) =>
  `${tag}-${Date.now().toString(36)}-${counter++}`;

// Reused from rerender.spec.js — the real sign-in flow, no shortcut. Optionally
// seeds localStorage before the app boots, which is how the AI key gets in
// front of `AiSettings::load()` on first paint.
async function signInAs(page, label, { seed } = {}) {
  await page.goto(BASE);
  if (seed) await page.evaluate(seed);
  const skip = page.getByRole("button", { name: /^skip$/i });
  if (await skip.count()) await skip.click().catch(() => {});
  await page.getByRole("button", { name: "English", exact: true }).click();
  await page.getByRole("tab", { name: "Private key" }).click();
  await page.getByRole("textbox", { name: "Username" }).fill(label);
  await page
    .getByRole("textbox", { name: "Private key" })
    .fill(walletFor(label).privateKey);
  await page.locator('button:text-is("Sign in")').click();
  await expect(page.getByRole("complementary", { name: "Rooms" })).toBeVisible({
    timeout: 20_000,
  });
}

// The sidebar the user sees. `nav_rooms` is the listbox's aria-label; each row
// is a `role="option"` carrying its rendered title in `data-name`, and each
// section heading is `.fn-room-section`.
const roomList = (page) => page.getByRole("listbox", { name: "Rooms" });

// Open a room by clicking its row and waiting for the chat header to name it.
async function openRoom(page, name) {
  await roomList(page).locator(`[data-name="${name}"]`).first().click();
  await expect(
    page.locator(".fn-chat__title").getByText(name, { exact: true }),
  ).toBeVisible({ timeout: 10_000 });
}

async function openMenu(page) {
  await page.getByRole("button", { name: "Room actions" }).click();
  await expect(page.getByRole("menu", { name: "Room actions" })).toBeVisible();
}

// The management verbs a built-in room refuses server-side, so its menu must
// not offer any of them (review bug 9). A normal channel offers all of them.
const MANAGEMENT_ITEMS = [
  "Invite people",
  "Invite links",
  "Rename room",
  "Manage admins",
  "Leave room",
  "Delete room",
];

test.describe("built-in rooms", () => {
  test("are provisioned, pinned and categorised", async ({ page, request }) => {
    const label = freshLabel("provision");
    // A normal channel, created as this wallet before the browser signs in, so
    // the sidebar has two categories and the "My rooms" heading is forced.
    const user = await signIn(request, label);
    const chanName = `Field ops ${label}`;
    await channel(request, user, chanName);

    await signInAs(page, label);

    // All three arrive without anybody creating them.
    for (const name of ["My Note", "My Jarvis", "My Lobby"]) {
      await expect(roomList(page).locator(`[data-name="${name}"]`)).toHaveCount(1);
    }
    // …and so does the channel.
    await expect(roomList(page).locator(`[data-name="${chanName}"]`)).toHaveCount(1);

    // The built-in three are pinned first, ahead of the channel.
    // `data-name` is on the option element itself, not a descendant.
    const names = await roomList(page)
      .getByRole("option")
      .evaluateAll((opts) => opts.map((o) => o.getAttribute("data-name")));
    expect(new Set(names.slice(0, 3))).toEqual(
      new Set(["My Note", "My Jarvis", "My Lobby"]),
    );
    expect(names).toContain(chanName);
    expect(names.indexOf(chanName)).toBeGreaterThan(2);

    // With two categories present, the headers render — the pinned group
    // first, then Channels. Compared case-insensitively because `.fn-room-section`
    // is uppercased by CSS `text-transform`, which `innerText` reflects: the
    // content is "My rooms", the render is "MY ROOMS", and it is the content
    // this test is about.
    const headingText = (await page.locator(".fn-room-section").allInnerTexts()).map(
      (h) => h.toLowerCase(),
    );
    expect(headingText[0]).toBe("my rooms");
    expect(headingText).toContain("channels");
  });

  test("My Note accepts a message through the composer", async ({ page }) => {
    const label = freshLabel("note");
    await signInAs(page, label);
    await openRoom(page, "My Note");

    const line = `a private thought ${label}`;
    const box = page.getByRole("textbox", { name: /^Message My Note$/ });
    await box.fill(line);
    await box.press("Enter");

    await expect(
      page.locator(".fn-bubble").getByText(line, { exact: true }),
    ).toBeVisible({ timeout: 10_000 });
  });

  test("a built-in room's menu hides the controls that would only error", async ({
    page,
    request,
  }) => {
    const label = freshLabel("menu");
    // A normal channel this wallet administers, created before browser sign-in.
    const user = await signIn(request, label);
    const chanName = `Team ${label}`;
    await channel(request, user, chanName);

    await signInAs(page, label);

    // The control: every management verb is legitimate in a channel you admin,
    // so every item must be present. If a regression removed one, this fails.
    await openRoom(page, chanName);
    await openMenu(page);
    for (const item of MANAGEMENT_ITEMS) {
      await expect(
        page.getByRole("menuitem", { name: item, exact: true }),
        `a normal channel must offer "${item}"`,
      ).toBeVisible();
    }
    await page.keyboard.press("Escape").catch(() => {});

    // The built-in room: every one of those verbs 400s server-side, so the
    // menu must not offer them. Hiding stays (it works and is reversible).
    await openRoom(page, "My Note");
    await openMenu(page);
    for (const item of MANAGEMENT_ITEMS) {
      await expect(
        page.getByRole("menuitem", { name: item, exact: true }),
        `a built-in room must NOT offer "${item}"`,
      ).toHaveCount(0);
    }
    await expect(
      page.getByRole("menuitem", { name: "Hide room", exact: true }),
    ).toBeVisible();
  });

  test("My Jarvis replies to the question just asked, and is badged AI", async ({
    page,
  }) => {
    const nonce = freshLabel("jx");
    const question = `PING-${nonce}`;
    const answer = `PONG-${nonce}`;

    // Intercept the provider call before the app can make it, and capture the
    // request body so the regression guard can read what was actually sent. The
    // browser fetches cross-origin (127.0.0.1 → api.x.ai) with a JSON body and
    // an Authorization header, so it first sends a CORS preflight — both the
    // OPTIONS and the POST are fulfilled here, with permissive CORS headers so
    // the browser hands the fulfilled response back to the page's JS.
    let sentBody = null;
    await page.route("https://api.x.ai/v1/chat/completions", async (route) => {
      const cors = {
        "Access-Control-Allow-Origin": "*",
        "Access-Control-Allow-Methods": "POST, OPTIONS",
        "Access-Control-Allow-Headers": "*",
      };
      if (route.request().method() === "OPTIONS") {
        await route.fulfill({ status: 204, headers: cors, body: "" });
        return;
      }
      sentBody = JSON.parse(route.request().postData() || "{}");
      await route.fulfill({
        status: 200,
        headers: { "Content-Type": "application/json", ...cors },
        body: JSON.stringify({
          choices: [{ message: { role: "assistant", content: answer } }],
        }),
      });
    });

    // Seed the AI settings the way `AiSettings::save()` writes them: JSON under
    // `ps-ai`, keys map keyed by provider id ("grok"), `text_provider` the
    // lowercased enum. The key value is never checked — the call is intercepted
    // — so any non-empty string makes `text_provider()` resolve to Some(Grok).
    await signInAs(page, freshLabel("jarvis"), {
      seed: () =>
        localStorage.setItem(
          "ps-ai",
          JSON.stringify({
            keys: { grok: "xai-e2e-not-a-real-key" },
            text_provider: "grok",
            image_provider: null,
          }),
        ),
    });

    await openRoom(page, "My Jarvis");
    const box = page.getByRole("textbox", { name: /^Message My Jarvis$/ });
    await box.fill(question);
    await box.press("Enter");

    // (a) the question appears, (b) the reply appears.
    await expect(
      page.locator(".fn-bubble").getByText(question, { exact: true }),
    ).toBeVisible({ timeout: 10_000 });
    await expect(
      page.locator(".fn-bubble").getByText(answer, { exact: true }),
    ).toBeVisible({ timeout: 15_000 });

    // (c) the reply is badged as an agent, not rendered as a person. Find the
    // message row holding the answer and assert it carries the AI badge.
    const replyRow = page
      .locator(".fn-msg")
      .filter({ has: page.locator(".fn-bubble", { hasText: answer }) });
    await expect(replyRow.locator(".fn-badge", { hasText: "AI" })).toBeVisible();

    // (d) THE REGRESSION GUARD. The stale-snapshot bug asked the model the
    // wrong turn. The last user message in the request the client actually sent
    // must be exactly the question just typed. Do not weaken this.
    expect(sentBody, "the provider was never called").not.toBeNull();
    const msgs = sentBody.messages || [];
    const lastUser = [...msgs].reverse().find((m) => m.role === "user");
    expect(lastUser, "no user message reached the provider").toBeTruthy();
    expect(lastUser.content).toContain(question);
  });

  test("My Jarvis shows the no-key note when no provider is configured", async ({
    page,
  }) => {
    // No `ps-ai` seeded, so `text_provider()` is None.
    await signInAs(page, freshLabel("nokey"));
    await openRoom(page, "My Jarvis");

    await expect(
      page.getByText(
        "Add an AI provider key in Settings and Jarvis will answer here.",
        { exact: true },
      ),
    ).toBeVisible();

    // Sending must not silently produce a reply — there is no provider to
    // answer. The message still posts (it is a real room), but no agent bubble
    // arrives, so no AI badge ever appears.
    const line = `into the void ${counter}`;
    const box = page.getByRole("textbox", { name: /^Message My Jarvis$/ });
    await box.fill(line);
    await box.press("Enter");
    await expect(
      page.locator(".fn-bubble").getByText(line, { exact: true }),
    ).toBeVisible({ timeout: 10_000 });
    await page.waitForTimeout(1500);
    await expect(page.locator(".fn-msg .fn-badge", { hasText: "AI" })).toHaveCount(0);
  });
});
