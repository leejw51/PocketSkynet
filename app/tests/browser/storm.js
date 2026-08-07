// The user's repro, replayed: upload a video over https, then do what Safari
// does to it — a storm of small Range requests — and then check the thing
// that actually broke on the phone: can this session still talk to the
// server afterwards, or has playback burned its rate-limit budget?
const { chromium } = require("playwright");
const fs = require("fs");

const VIDEO = process.env.VIDEO || "/tmp/ps-small-movie.mp4";
const BASE = process.env.BASE || "https://127.0.0.1:9099";
const STORM = Number(process.env.STORM || 150);

async function main() {
  const browser = await chromium.launch();
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await ctx.newPage();
  const errors = [];
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(m.text());
  });
  page.on("pageerror", (e) => errors.push("pageerror: " + e.message));

  await page.goto(BASE + "/", { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(4000);
  const en = page.getByRole("button", { name: "English" }).first();
  if (await en.isVisible().catch(() => false)) {
    await en.click();
    await page.waitForTimeout(800);
  }
  await page
    .getByRole("button", { name: /Create a wallet and sign in/i })
    .click();
  await page.waitForTimeout(2500);
  const copy = page.getByRole("button", { name: /Copy phrase/i }).first();
  if (await copy.isVisible().catch(() => false)) {
    await ctx
      .grantPermissions(["clipboard-read", "clipboard-write"])
      .catch(() => {});
    await copy.click();
    await page.waitForTimeout(800);
  }
  const cont = page
    .getByRole("button", { name: /Save the phrase to continue/i })
    .first();
  if (await cont.isVisible().catch(() => false)) {
    await cont.click();
    await page.waitForTimeout(1500);
  }
  const signIn = page.getByRole("button", { name: /^Sign in$/i }).first();
  if (await signIn.isEnabled().catch(() => false)) await signIn.click();
  for (let i = 0; i < 40; i++) {
    if ((await page.locator("input[type=file]").count()) > 0) break;
    await page.keyboard.press("Escape").catch(() => {});
    await page.mouse.click(640, 400).catch(() => {});
    await page.waitForTimeout(1000);
  }
  const fast = page.getByRole("button", { name: /Fast create room/i }).first();
  if (await fast.isVisible().catch(() => false)) {
    await fast.click();
    await page.waitForTimeout(4000);
  }

  // 1. Upload the video.
  console.log(`uploading ${VIDEO} (${fs.statSync(VIDEO).size} B)...`);
  await page
    .locator(".fn-composer input[type=file]")
    .first()
    .setInputFiles(VIDEO);
  let seen = false,
    gone = 0;
  for (let i = 0; i < 480; i++) {
    const vis = await page
      .locator(".fn-transfers")
      .isVisible()
      .catch(() => false);
    if (vis) {
      seen = true;
      gone = 0;
    } else if (seen && ++gone >= 8) break;
    await page.waitForTimeout(250);
  }
  await page.waitForTimeout(4000);
  const thumb = await page.locator(".fn-attach__play").count();
  console.log(`uploaded; thumbnail rendered: ${thumb > 0}`);

  // 2. The storm — what Safari's media loader does to an mp4, compressed
  //    into one loop. Every request carries the capability URL the page's
  //    own player uses.
  const src = await page
    .locator(".fn-attach__play video")
    .first()
    .getAttribute("src");
  console.log("media src present:", !!src);
  const storm = await page.evaluate(
    async ({ src, n }) => {
      const statuses = {};
      for (let i = 0; i < n; i++) {
        try {
          const r = await fetch(src, {
            headers: { Range: `bytes=${i * 17}-${i * 17 + 63}` },
          });
          statuses[r.status] = (statuses[r.status] || 0) + 1;
        } catch (e) {
          statuses["network"] = (statuses["network"] || 0) + 1;
        }
      }
      return statuses;
    },
    { src, n: STORM },
  );
  console.log(`storm of ${STORM} range requests:`, JSON.stringify(storm));

  // 3. The victim: can this session still do ordinary things?
  const composer = page.locator(".fn-composer textarea");
  await composer.fill("still alive after the storm");
  await composer.press("Enter");
  await page.waitForTimeout(6000);
  const posted = await page.locator("text=still alive after the storm").count();
  console.log(`message posted after the storm: ${posted > 0}`);

  const toasts = await page
    .locator('.fn-toast, [class*="toast"]')
    .allInnerTexts()
    .catch(() => []);
  const throttled = toasts.filter((t) => /too many/i.test(t));
  console.log("throttle toasts:", JSON.stringify(throttled));
  console.log(
    "console errors:",
    JSON.stringify(errors.filter((e) => !/429/.test(e)).slice(0, 5)),
  );
  await page.screenshot({ path: "storm-after.png" });

  const pass =
    thumb > 0 &&
    (storm["206"] || 0) === STORM &&
    posted > 0 &&
    throttled.length === 0;
  console.log(pass ? "STORM SURVIVED ✓" : "STORM BROKE THE SESSION ✗");
  await browser.close();
  process.exit(pass ? 0 : 1);
}

main().catch((e) => {
  console.error("FAILED", e);
  process.exit(1);
});
