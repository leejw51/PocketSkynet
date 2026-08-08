// Every destination is reachable on a tablet held upright.
//
// The top bar drops everything tagged `--wide` below 800px, on the reasoning
// that the bottom nav and the More sheet carry those sections instead. That
// reasoning is only true if More really does list them, and an iPad in
// portrait is the width where it matters most: wide enough that nobody thinks
// of it as a phone, narrow enough that the whole top row is gone.
//
// Reported against Skynet Password specifically, so that one is asserted by
// name, but the loop is the real guard: any section that leaves the top bar
// without arriving in More becomes unreachable, and nothing else would notice.

const { test, expect } = require("@playwright/test");
const { BASE, walletFor } = require("./helpers");

// iPad mini in portrait: 744×1133. This is the width the report came from —
// wide enough that nobody calls it a phone, and just under the 800px line
// where the whole `--wide` top-bar row disappears.
test.use({ viewport: { width: 744, height: 1133 }, hasTouch: true });

let counter = 0;
const freshLabel = (tag) => `${tag}-${Date.now().toString(36)}-${counter++}`;

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

// The sections the top bar hands to More below 800px. `IN_MORE` in shell.rs is
// the list this mirrors.
const BEHIND_MORE = [
  "Skynet Password",
  "Bank",
  "Knowledge",
  "Publish",
  "Invitations",
  "Settings",
];

test.describe("on a tablet in portrait", () => {
  test("the wide top-bar row really is gone", async ({ page }) => {
    // The premise. If this ever fails the rest of the file is testing nothing,
    // because the sections would still be reachable up top.
    await signInAs(page, freshLabel("tabp"));
    await expect(page.locator(".fn-bottomnav")).toBeVisible();
    await expect(
      page.getByRole("button", { name: "Skynet Password" }),
    ).toBeHidden();
  });

  test("More reaches every section the top bar gave up", async ({ page }) => {
    await signInAs(page, freshLabel("tabm"));

    await page.getByRole("button", { name: "More" }).click();
    const sheet = page.getByRole("dialog");
    await expect(sheet).toBeVisible();

    for (const name of BEHIND_MORE) {
      await expect(
        sheet.getByRole("button", { name, exact: true }),
        `${name} is not reachable on a tablet in portrait`,
      ).toBeVisible();
    }
  });

  test("Skynet Password actually opens from More", async ({ page }) => {
    // Visible in a list is not the same as reachable: the row has to navigate,
    // and the screen behind it has to render at this width.
    await signInAs(page, freshLabel("tabo"));

    await page.getByRole("button", { name: "More" }).click();
    await page
      .getByRole("dialog")
      .getByRole("button", { name: "Skynet Password", exact: true })
      .click();

    await expect(page).toHaveURL(/\/passwords$/);
    await expect(
      page.getByRole("heading", { name: "Skynet Password" }),
    ).toBeVisible({ timeout: 10_000 });
  });
});
