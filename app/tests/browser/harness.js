// A throwaway server for a hermetic browser test, plus the sign-in flow every
// such test needs first. Mirrors `tests/integration/supervisor.py::Backend` —
// same contract (own port, own data dir, torn down in `finally` no matter how
// the test went), reimplemented in Node because Playwright is Node.
//
// Unlike that Python harness, `staticDir` here is the *real* built UI
// (`web/dist`), not an empty directory — a browser test exercises the actual
// bundle, not just the API.
const { spawn } = require("child_process");
const fs = require("fs");
const os = require("os");
const net = require("net");
const path = require("path");
const https = require("https");

const APP_DIR = path.join(__dirname, "..", "..");
const DEFAULT_BIN = path.join(APP_DIR, "target", "release", "pocketskynet");
const STATIC_DIR = path.join(APP_DIR, "web", "dist");
const BOOT_TIMEOUT_MS = 30000;

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
    srv.on("error", reject);
  });
}

// Verifies against the CA the server mints on first boot, same as the Python
// harness (`ssl.create_default_context(cafile=...)`) — the point of this
// probe succeeding is proving that file is on disk and valid, which a
// verification bypass would throw away.
function waitHealthy(baseUrl, caPath, child) {
  const deadline = Date.now() + BOOT_TIMEOUT_MS;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      if (child.exitCode !== null) {
        reject(new Error(`server exited during boot (code ${child.exitCode})`));
        return;
      }
      let ca;
      try {
        ca = fs.readFileSync(caPath);
      } catch {
        retry(); // not flushed to disk yet
        return;
      }
      const req = https.get(
        `${baseUrl}/api/health`,
        { ca, timeout: 2000 },
        (res) => {
          res.resume();
          if (res.statusCode === 200) resolve();
          else retry();
        },
      );
      req.on("error", retry);
      req.on("timeout", () => {
        req.destroy();
        retry();
      });
    };
    const retry = () => {
      if (Date.now() > deadline) {
        reject(
          new Error(`server did not answer ${baseUrl}/api/health in time`),
        );
        return;
      }
      setTimeout(attempt, 100);
    };
    attempt();
  });
}

// Boots a fresh server on its own port and data directory, over TLS (a
// self-signed CA it mints on first run) — required, since sign-in needs
// `crypto.subtle`, which only exists in a secure context. Returns
// `{ baseUrl, stop() }`; `stop()` is safe to call more than once and is
// expected to run in a `finally`.
async function bootServer() {
  const binary = process.env.POCKETSKYNET_BIN || DEFAULT_BIN;
  if (!fs.existsSync(binary)) {
    throw new Error(
      `server binary not found: ${binary} (run 'make build' first)`,
    );
  }
  if (!fs.existsSync(STATIC_DIR)) {
    throw new Error(`${STATIC_DIR} not found (run 'make build' first)`);
  }

  const port = await freePort();
  const redirectPort = await freePort();
  const root = fs.mkdtempSync(
    path.join(os.tmpdir(), "pocketskynet-chunktest-"),
  );
  const dataDir = path.join(root, "data");
  fs.mkdirSync(dataDir, { recursive: true });
  const logPath = path.join(root, "server.log");
  const log = fs.openSync(logPath, "w");

  // Same isolation contract as the Python harness: only what this test
  // decides reaches the server, not whatever the surrounding shell exports.
  const env = {};
  for (const [k, v] of Object.entries(process.env)) {
    if (!/^(PS_|VITE_|POCKETSKYNET_)/.test(k)) env[k] = v;
  }
  env.PS_IGNORE_BAKED_ENV = "1";

  const args = [
    "--host",
    "127.0.0.1",
    "--port",
    String(port),
    "--data-dir",
    dataDir,
    "--static-dir",
    STATIC_DIR,
    "--no-rate-limit",
    "--no-mdns",
    "--log",
    "warn",
    "--tls",
    "--http-redirect-port",
    String(redirectPort),
  ];
  const child = spawn(binary, args, { env, stdio: ["ignore", log, log] });

  const baseUrl = `https://127.0.0.1:${port}`;
  let stopped = false;
  // Waits for the process to actually exit before clearing its directory —
  // removing it while the binary still holds the sqlite file or the TLS
  // material open is a race (ENOTEMPTY/EBUSY), not a cleanup nicety.
  const stop = async () => {
    if (stopped) return;
    stopped = true;
    if (child.exitCode === null) {
      const exited = new Promise((resolve) => child.once("exit", resolve));
      child.kill("SIGTERM");
      const timedOut = await Promise.race([
        exited.then(() => false),
        new Promise((resolve) => setTimeout(() => resolve(true), 6000)),
      ]);
      if (timedOut && child.exitCode === null) {
        child.kill("SIGKILL");
        await exited;
      }
    }
    fs.rmSync(root, { recursive: true, force: true });
  };

  const caPath = path.join(dataDir, "tls", "ca.crt");
  try {
    await waitHealthy(baseUrl, caPath, child);
  } catch (e) {
    try {
      const tail = fs.readFileSync(logPath, "utf8").slice(-4000);
      if (tail)
        console.error(
          "---- server log (tail) ----\n" +
            tail +
            "\n----------------------------",
        );
    } catch {}
    await stop();
    throw e;
  }

  return { baseUrl, stop };
}

// The sign-in-and-create-a-room flow duplicated across upload.js/resume.js,
// factored out for the new hermetic test rather than copied a third time.
async function signInAndCreateRoom(page, ctx, baseUrl) {
  await page.goto(baseUrl + "/", { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(4000);

  await page
    .getByRole("button", { name: /Create a wallet and sign in/i })
    .click();
  await page.waitForTimeout(2500);

  // The phrase gate: real, and the app will not continue past it until the
  // phrase has been copied or downloaded.
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

  const uname = page.locator("input[type=text]").first();
  if (await uname.isVisible().catch(() => false)) {
    const v = await uname.inputValue().catch(() => "");
    if (!v) {
      await uname.fill("chunktest");
      await page.waitForTimeout(400);
    }
  }
  const signIn = page.getByRole("button", { name: /^Sign in$/i }).first();
  if (await signIn.isEnabled().catch(() => false)) {
    await signIn.click();
  }

  // The boot cutscene sits between sign-in and the app; click through it.
  for (let i = 0; i < 40; i++) {
    if ((await page.locator("input[type=file]").first().count()) > 0) break;
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
    const row = page.locator('[class*="fn-room-row"], li').first();
    if (await row.isVisible().catch(() => false)) {
      await row.click();
      await page.waitForTimeout(2500);
    }
  }
}

module.exports = { bootServer, signInAndCreateRoom };
