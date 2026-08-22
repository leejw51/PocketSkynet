// Diagnostic (not part of the suite): drive a 2-of-2 TSS keygen and narrate
// everything observable — console, page errors, failed requests, and the
// wizard's step list — so a silent worker failure and a merely-slow prime
// hunt stop looking identical.
const { chromium } = require("playwright");
const { bootServer } = require("./harness");

const PASSPHRASE = "browser walkthrough passphrase";

async function main() {
  const server = await bootServer();
  let browser;
  try {
    browser = await chromium.launch({ args: ["--ignore-certificate-errors"] });
    const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
    const page = await ctx.newPage();
    page.on("console", (m) => console.log(`[console.${m.type()}]`, m.text()));
    page.on("pageerror", (e) => console.log("[pageerror]", e.message));
    page.on("requestfailed", (r) =>
      console.log("[requestfailed]", r.url(), r.failure()?.errorText),
    );
    page.on("worker", (w) => console.log("[worker created]", w.url()));

    await page.goto(server.baseUrl + "/login", {
      waitUntil: "domcontentloaded",
    });
    await page.locator("#tab-tss").waitFor({ timeout: 60000 });
    await page.locator("#tab-tss").click();

    // Smallest legal wallet: 2-of-2 — two prime sets instead of three.
    await page
      .locator("#login-tss-parties")
      .selectOption("2")
      .catch(async () => {
        await page.locator("#login-tss-parties").fill("2");
      });
    await page.locator("#login-tss-newpass").fill(PASSPHRASE);
    await page.locator("#login-tss-newpass2").fill(PASSPHRASE);
    await page.getByRole("button", { name: "Run key generation" }).click();

    const started = Date.now();
    const deadline = started + 25 * 60 * 1000;
    let lastSteps = "";
    while (Date.now() < deadline) {
      const err = await page
        .locator(".fn-login__error")
        .innerText()
        .catch(() => "");
      if (err.trim()) {
        console.log("KEYGEN ERROR SHOWN:", err.trim());
        return;
      }
      if ((await page.locator(".fn-tss-backup").count()) > 0) {
        console.log(
          `BACKUP PANEL after ${((Date.now() - started) / 1000) | 0}s`,
        );
        return;
      }
      const steps = await page
        .locator(".fn-tss-steps li")
        .evaluateAll((els) =>
          els
            .map((e) => `${e.getAttribute("data-state")}:${e.innerText}`)
            .join(" | "),
        )
        .catch(() => "(no step list)");
      if (steps !== lastSteps) {
        console.log(`[${((Date.now() - started) / 1000) | 0}s]`, steps);
        lastSteps = steps;
      }
      await page.waitForTimeout(5000);
    }
    console.log("TIMED OUT with steps:", lastSteps);
  } finally {
    if (browser) await browser.close().catch(() => {});
    await server.stop();
  }
}

main().catch((e) => {
  console.error(e.stack || String(e));
  process.exit(1);
});
