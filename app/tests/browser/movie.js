// Upload a real film and prove three things about it:
//
//   1. it uploads at all, at a size the old cap forbade by a factor of 35;
//   2. the message shows a *thumbnail*, and drawing it costs a fraction of the
//      file rather than the file;
//   3. clicking it plays, and seeking works — which is only true if the server
//      is answering Range requests.
const { chromium } = require("playwright");
const fs = require("fs");

const MOVIE = process.env.MOVIE || "/tmp/ps-movie.mp4";

async function main() {
  const size = fs.statSync(MOVIE).size;
  console.log(`MOVIE ${MOVIE} ${(size / 1024 / 1024).toFixed(0)} MB`);

  const browser = await chromium.launch();
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await ctx.newPage();
  const errors = [];
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(m.text());
  });
  page.on("pageerror", (e) => errors.push("pageerror: " + e.message));

  // How many bytes the browser actually pulls back for media, so "thumbnail,
  // not download" is measured rather than asserted.
  let mediaBytes = 0;
  let rangeRequests = 0;
  page.on("response", async (r) => {
    const u = new URL(r.url());
    if (!u.pathname.includes("/raw")) return;
    if (r.status() === 206) rangeRequests++;
    const len = Number(r.headers()["content-length"] || 0);
    if (Number.isFinite(len)) mediaBytes += len;
  });

  await page.goto("https://127.0.0.1:9099/", { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(4000);
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
  if ((await page.locator("input[type=file]").count()) === 0) {
    await page
      .locator('[class*="fn-room-row"], li')
      .first()
      .click()
      .catch(() => {});
    await page.waitForTimeout(2500);
  }

  console.log("uploading the film...");
  await page.locator("input[type=file]").first().setInputFiles(MOVIE);

  let sawRail = false,
    goneFor = 0,
    maxPct = 0;
  for (let i = 0; i < 9600; i++) {
    const vis = await page
      .locator(".fn-transfers")
      .isVisible()
      .catch(() => false);
    if (vis) {
      sawRail = true;
      goneFor = 0;
      const t = (
        await page
          .locator(".fn-transfers")
          .innerText()
          .catch(() => "")
      ).replace(/\s+/g, " ");
      const m = t.match(/(\d+)%/);
      if (m) maxPct = Math.max(maxPct, Number(m[1]));
    } else if (sawRail && ++goneFor >= 8) break;
    await page.waitForTimeout(250);
  }
  console.log(`upload finished (rail reached ${maxPct}%)`);
  await page.waitForTimeout(6000);

  // Let the poster settle, then measure what the thumbnail cost.
  mediaBytes = 0;
  await page.waitForTimeout(6000);
  const posterBytes = mediaBytes;
  const poster = page.locator(".fn-attach__play").first();
  const hasThumb = await poster.isVisible().catch(() => false);
  console.log(`THUMBNAIL shown: ${hasThumb}`);
  console.log(
    `THUMBNAIL cost: ${(posterBytes / 1024 / 1024).toFixed(1)} MB of a ${(size / 1024 / 1024).toFixed(0)} MB film`,
  );
  await page.screenshot({ path: "movie-thumb.png" });

  // No player until asked.
  const playersBefore = await page.locator("video[controls]").count();
  console.log(`players before the click: ${playersBefore}`);

  if (hasThumb) {
    await poster.click();
    await page.waitForTimeout(8000);
  }
  const playersAfter = await page.locator("video[controls]").count();
  console.log(`players after the click: ${playersAfter}`);

  // Does it actually play, and can it seek? Seeking is the Range test.
  const state = await page.evaluate(async () => {
    const v = document.querySelector("video[controls]");
    if (!v) return null;
    await new Promise((r) => setTimeout(r, 3000));
    const before = { t: v.currentTime, ready: v.readyState, dur: v.duration };
    v.currentTime = Math.max(0, (v.duration || 0) - 30);
    await new Promise((r) => setTimeout(r, 5000));
    return {
      before,
      after: { t: v.currentTime, ready: v.readyState },
      dur: v.duration,
    };
  });
  console.log("PLAYBACK:", JSON.stringify(state));
  console.log(`206 Partial Content responses: ${rangeRequests}`);
  await page.screenshot({ path: "movie-playing.png" });
  console.log("CONSOLE ERRORS:", JSON.stringify(errors.slice(0, 6)));

  const seeked = state && state.after.t > 60 && state.after.ready >= 1;
  console.log(seeked ? "SEEK INTO A 10-MINUTE FILM WORKED ✓" : "SEEK FAILED ✗");
  await browser.close();
}

main().catch((e) => {
  console.error("FAILED", e);
  process.exit(1);
});
