// Deleting a message takes its picture with it.
//
// The bug: an attachment is its own row in `files`, with no column naming the
// message that displayed it — the link is the URL inside the message text, and
// in an encrypted room the server holds ciphertext and can never read it. So
// `DELETE /api/messages/{id}` removed the message and left the file, and both
// the Files drawer and the gallery read `files` directly. The image was
// "deleted" and still on screen.
//
// Only the client holds the key, so only the client can close that gap
// (`dialogs/delete_message.rs::remove_attachments`) — which is why this is a
// browser test and not a server one. The room is set up over the API, the way
// rerender.spec.js sets up its channels, and the *deletion* is done by
// clicking, because clicking is the thing that was broken.

const { test, expect } = require("@playwright/test");
const {
  BASE,
  signIn,
  api,
  json,
  channel,
  post,
  walletFor,
} = require("./helpers");

let counter = 0;
const freshLabel = (tag) => `${tag}-${Date.now().toString(36)}-${counter++}`;

// The smallest thing the server will accept as a picture: a 1x1 PNG. The
// gallery filters on the stored extension, so it has to really be a .png.
const PNG_1X1 = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
  "base64",
);

async function signInAs(page, label) {
  await page.goto(BASE);
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

/** Everything the gallery would draw for a room. */
async function mediaIds(request, user, room) {
  const page = await json(
    await api(request, user).get(`/api/rooms/${room}/media`),
  );
  return (page.items || page.files || []).map((f) => f.id);
}

test.describe("attachments and the gallery", () => {
  test("deleting the message deletes the picture it was showing", async ({
    page,
    request,
  }) => {
    const label = freshLabel("att");
    const user = await signIn(request, label);
    const room = await channel(request, user, `gallery-${label}`);

    // Upload, then post a message that names it — which is exactly the pair the
    // composer produces.
    const uploaded = await json(
      await request.post(
        `${BASE}/api/rooms/${room}/files?filename=shot.png&caption=`,
        {
          headers: {
            ...user.auth,
            "Content-Type": "application/octet-stream",
          },
          data: PNG_1X1,
        },
      ),
    );
    expect(
      uploaded.id,
      `upload failed: ${JSON.stringify(uploaded)}`,
    ).toBeTruthy();
    await post(request, user, room, `/api/files/${uploaded.id}/raw`);

    // Precondition: the gallery has it. Without this the test could pass by
    // never having put anything there.
    expect(await mediaIds(request, user, room)).toContain(uploaded.id);

    // Now delete the message the way a person does.
    await signInAs(page, label);
    await page
      .getByRole("listbox", { name: "Rooms" })
      .locator(`[data-name="gallery-${label}"]`)
      .first()
      .click();
    const row = page.locator(".fn-msg").first();
    await expect(row).toBeVisible({ timeout: 15_000 });
    await row.hover();
    await row.getByRole("button", { name: /More actions/ }).click();
    await page.getByRole("menuitem", { name: "Delete" }).click();
    // The confirm dialog. Its own Delete button, not the menu's.
    await page
      .getByRole("dialog")
      .getByRole("button", { name: "Delete" })
      .click();

    // The row goes first (the dissolve), and the file follows.
    await expect(page.locator(".fn-msg")).toHaveCount(0, { timeout: 20_000 });
    await expect
      .poll(() => mediaIds(request, user, room), { timeout: 20_000 })
      .not.toContain(uploaded.id);
  });

  test("deleting a message with no attachment deletes nothing else", async ({
    page,
    request,
  }) => {
    // The guard on the other side: a false positive here would delete a file
    // the message never showed. Two messages, one carrying the picture and one
    // not; deleting the plain one must leave the picture alone.
    const label = freshLabel("keep");
    const user = await signIn(request, label);
    const room = await channel(request, user, `keep-${label}`);

    const uploaded = await json(
      await request.post(
        `${BASE}/api/rooms/${room}/files?filename=keep.png&caption=`,
        {
          headers: { ...user.auth, "Content-Type": "application/octet-stream" },
          data: PNG_1X1,
        },
      ),
    );
    await post(request, user, room, `/api/files/${uploaded.id}/raw`);
    await post(request, user, room, "just a sentence");

    await signInAs(page, label);
    await page
      .getByRole("listbox", { name: "Rooms" })
      .locator(`[data-name="keep-${label}"]`)
      .first()
      .click();
    // The plain one is last.
    const row = page.locator(".fn-msg").last();
    await expect(row).toBeVisible({ timeout: 15_000 });
    await row.hover();
    await row.getByRole("button", { name: /More actions/ }).click();
    await page.getByRole("menuitem", { name: "Delete" }).click();
    await page
      .getByRole("dialog")
      .getByRole("button", { name: "Delete" })
      .click();

    await expect(page.locator(".fn-msg")).toHaveCount(1, { timeout: 20_000 });
    expect(await mediaIds(request, user, room)).toContain(uploaded.id);
  });
});
