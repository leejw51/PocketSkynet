// Does the TSS wallet actually work the way a person meets it?
//
// The Rust suite already proves the cryptography end to end:
// `tss/tests/dkg_sign.rs` runs the real 2-of-3 DKLs23 ceremony and signs
// with two different quorums. What it cannot see is the WASM client, which
// since the move to in-browser ceremonies IS the whole feature: the create
// wizard driving the DKG in the tss_worker Web Worker, the
// download-every-share gate, the file picker's quorum arithmetic, and the
// sign-in whose challenge signature is minted by a local ceremony and only
// then presented to `/api/auth/login`. A bug in any of those ships a
// wallet nobody can create or reopen while every Rust test stays green.
//
// So this walks the whole story through a real browser, hermetically
// (harness.js boots its own server, torn down in `finally`):
//
//   1. create a 2-of-3 wallet in the UI and watch the ceremony finish;
//   2. hit the backup gate: sign-in stays locked until every share file
//      is downloaded, and the downloads are real files;
//   3. sign in with the freshly minted wallet;
//   4. sign out, then log back in with only shares 1 and 3 — the
//      lost-share case the whole feature exists for — and land in the
//      same account;
//   5. confirm one share alone never unlocks the button.
//
// A 2-of-3 DKLs23 DKG is seconds of OT arithmetic even in browser wasm;
// the polls below stay generous anyway — a timeout margin has never
// broken a test.
const { chromium } = require("playwright");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { bootServer } = require("./harness");

const PASSPHRASE = "browser walkthrough passphrase";
const KEYGEN_TIMEOUT_MS = 5 * 60 * 1000;
const SIGNIN_TIMEOUT_MS = 3 * 60 * 1000;

async function openTssTab(page, baseUrl) {
  await page.goto(baseUrl + "/login", { waitUntil: "domcontentloaded" });
  // The WASM bundle needs a beat to boot before the tabs exist.
  await page.locator("#tab-tss").waitFor({ timeout: 60000 });
  await page.locator("#tab-tss").click();
}

// The boot cutscene sits between sign-in and the app; click through it and
// wait for the top bar to prove a real session, then return the signed-in
// wallet address from its accessible name.
async function waitSignedIn(page) {
  const deadline = Date.now() + SIGNIN_TIMEOUT_MS;
  const identity = page.locator(".fn-topbar__identity");
  while (Date.now() < deadline) {
    if ((await identity.count()) > 0) {
      const label = await identity.getAttribute("aria-label");
      const m = /0x[0-9a-fA-F]{40}/.exec(label || "");
      if (m) return m[0].toLowerCase();
    }
    await page.keyboard.press("Escape").catch(() => {});
    await page.mouse.click(640, 400).catch(() => {});
    await page.waitForTimeout(1000);
  }
  throw new Error("sign-in did not reach the app in time");
}

async function main() {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "ps-tss-browser-"));
  const server = await bootServer();
  let browser;
  try {
    browser = await chromium.launch({ args: ["--ignore-certificate-errors"] });
    const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
    const page = await ctx.newPage();
    // Page errors are WASM panics or broken JS — always a failure. Console
    // errors are not collected: the signed-out screen legitimately logs
    // 401s while probing the session.
    const pageErrors = [];
    page.on("pageerror", (e) => pageErrors.push(e.message));

    // ---- 1. create a 2-of-3 wallet through the wizard -----------------
    await openTssTab(page, server.baseUrl);
    // n defaults to 3 and m to 2 — assert rather than assume, since this
    // is also the shape the rest of the test banks on.
    if ((await page.locator("#login-tss-parties").inputValue()) !== "3")
      throw new Error("expected the create form to default to 3 parties");
    if ((await page.locator("#login-tss-threshold").inputValue()) !== "2")
      throw new Error("expected the create form to default to threshold 2");
    await page.locator("#login-tss-newpass").fill(PASSPHRASE);
    await page.locator("#login-tss-newpass2").fill(PASSPHRASE);
    await page.getByRole("button", { name: "Run key generation" }).click();

    // The ceremony narrates itself; the act list must be on screen.
    await page.locator(".fn-tss-steps").waitFor({ timeout: 15000 });

    // ---- 2. the backup gate ------------------------------------------
    await page
      .locator(".fn-tss-backup")
      .waitFor({ timeout: KEYGEN_TIMEOUT_MS });
    const address = (
      await page.locator(".fn-tss-backup .fn-mono").innerText()
    ).trim();
    if (!/^0x[0-9a-f]{40}$/.test(address))
      throw new Error(`backup panel shows no wallet address: "${address}"`);

    const cards = page.locator(".fn-tss-share");
    if ((await cards.count()) !== 3)
      throw new Error(`expected 3 share cards, got ${await cards.count()}`);

    const signInNow = page.getByRole("button", {
      name: "Sign in with this wallet",
    });
    if (await signInNow.isEnabled())
      throw new Error("sign-in must stay locked until every share is saved");

    const shareFiles = [];
    for (let i = 0; i < 3; i++) {
      const downloadP = page.waitForEvent("download", { timeout: 15000 });
      await cards.nth(i).getByRole("button").click();
      const download = await downloadP;
      const at = path.join(scratch, download.suggestedFilename());
      await download.saveAs(at);
      shareFiles.push(at);
      const parsed = JSON.parse(fs.readFileSync(at, "utf8"));
      if (parsed.type !== "pocketskynet-tss-share" || parsed.partyIndex !== i)
        throw new Error(`share ${i} downloaded with the wrong header`);
      if (parsed.address !== address)
        throw new Error(`share ${i} names a different wallet`);
    }
    if (!(await signInNow.isEnabled()))
      throw new Error("all shares saved, but sign-in is still locked");

    // ---- 3. sign in with the freshly minted wallet -------------------
    await signInNow.click();
    const signedInAs = await waitSignedIn(page);
    if (signedInAs !== address)
      throw new Error(`signed in as ${signedInAs}, expected ${address}`);

    // ---- 4. sign out, lose share 2, come back with 1 and 3 -----------
    await page.locator('button[aria-label="Sign out"]').click();
    await openTssTab(page, server.baseUrl);

    // One share is not a quorum: the button must know before the server is
    // ever asked.
    await page.locator("#login-tss-files").setInputFiles([shareFiles[0]]);
    await page.locator(".fn-tss-filelist li").first().waitFor({
      timeout: 15000,
    });
    const cta = page.locator(
      ".fn-login__actions button.topcoat-button--large--cta",
    );
    if (await cta.isEnabled())
      throw new Error("one share of a 2-of-3 wallet must not enable sign-in");

    await page
      .locator("#login-tss-files")
      .setInputFiles([shareFiles[0], shareFiles[2]]);
    await page.locator(".fn-tss-quorum-ok").waitFor({ timeout: 15000 });
    await page.locator("#login-tss-passphrase").fill(PASSPHRASE);
    if (!(await cta.isEnabled()))
      throw new Error("a full quorum with a passphrase must enable sign-in");
    await cta.click();

    const backAs = await waitSignedIn(page);
    if (backAs !== address)
      throw new Error(
        `lost-share login reached ${backAs}, expected ${address}`,
      );

    if (pageErrors.length)
      throw new Error("page errors during the run:\n" + pageErrors.join("\n"));

    console.log(
      `OK: created 2-of-3 wallet ${address}, backup-gated all 3 shares, ` +
        "signed in, then signed back in with shares {1,3} only",
    );
  } finally {
    if (browser) await browser.close().catch(() => {});
    await server.stop();
    fs.rmSync(scratch, { recursive: true, force: true });
  }
}

main().catch((e) => {
  console.error(e.stack || String(e));
  process.exit(1);
});
