// Does an upload actually go up in 500 KiB pieces?
//
// `server/tests/uploads.rs` already asserts `begin` *advertises* 500 KiB
// (`chunkSize`) — that is a protocol-level check with no browser involved.
// What that test structurally cannot see is whether the real WASM client
// *obeys* the advertisement: it reads `session.chunk_size` off a JSON
// response and slices a `web_sys::Blob` by it, and a bug there (a stale
// fallback, a unit mismatch, an off-by-one in the slice bounds) would pass
// every Rust test while still shipping the wrong chunk size to a phone on a
// lossy link. This drives one small upload through a real browser and reads
// the wire, the way `upload.js`/`resume.js` do for the rest of the protocol.
//
// Unlike those two, this test boots its own server (see harness.js) rather
// than requiring `make restart` first, and uses a small file — the property
// under test (chunk size) does not get truer at 120 MB, so there is no
// reason to pay for a large one. That is what makes it cheap enough to run
// from plain `make test`.
const { chromium } = require("playwright");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { bootServer, signInAndCreateRoom } = require("./harness");

// 500 KiB is `SUGGESTED_CHUNK_BYTES` (server/src/routes/uploads.rs),
// matching the reference implementation's `CHUNK_HINT` at
// test-upload/myrust/src/main.rs. Not imported directly since this runs
// outside the Rust build; kept as a literal here and asserted against the
// server's own advertised value below, so a drift between the two constants
// fails loudly rather than silently.
const CHUNK_BYTES = 500 * 1024;
// A little over four chunks, so the final short chunk is always exercised.
const FILE_BYTES = CHUNK_BYTES * 4 + 137 * 1024;

function makeFile(at) {
  const buf = Buffer.alloc(FILE_BYTES);
  for (let i = 0; i < buf.length; i++) buf[i] = (i * 31) % 251;
  fs.writeFileSync(at, buf);
}

async function main() {
  const scratchDir = fs.mkdtempSync(path.join(os.tmpdir(), "ps-chunksize-"));
  const filePath = path.join(scratchDir, "chunk-check.bin");
  makeFile(filePath);

  const server = await bootServer();
  let browser;
  try {
    // `ignoreHTTPSErrors` alone does not cover service-worker script fetches
    // (the app registers one) — Chromium still enforces certificate
    // validation for those unless the flag is also passed at launch.
    browser = await chromium.launch({ args: ["--ignore-certificate-errors"] });
    const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
    const page = await ctx.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(e.message));
    page.on("console", (m) => {
      if (m.type() === "error") errors.push(m.text());
    });

    // The chunk size the server advertises. Read off the wire rather than
    // imported, so this test also catches a client that ignores it.
    let advertisedChunkSize = null;
    // Neither `postDataBuffer()` nor `Content-Length`/`request.sizes()`
    // capture the byte count for these requests: the chunk body is a
    // `web_sys::Blob` slice (deliberately — see web/src/api/uploads.rs's
    // module doc), and Chromium does not surface a Blob-typed fetch body's
    // size through any of Playwright's usual channels. The offset itself is
    // enough, though — the server's own concurrency guard (`db/uploads.rs::
    // advance`) makes offset the authoritative byte count already received,
    // so the gap between consecutive offsets *is* each chunk's size.
    const offsets = [];
    page.on("request", (req) => {
      if (req.method() !== "PATCH") return;
      const url = new URL(req.url());
      if (!/\/api\/uploads\/[^/]+$/.test(url.pathname)) return;
      offsets.push(Number(url.searchParams.get("offset")));
    });
    page.on("response", async (res) => {
      const url = new URL(res.url());
      if (
        url.pathname === "/api/uploads" &&
        res.request().method() === "POST"
      ) {
        try {
          const json = await res.json();
          if (typeof json.chunkSize === "number")
            advertisedChunkSize = json.chunkSize;
        } catch {}
      }
    });

    await signInAndCreateRoom(page, ctx, server.baseUrl);

    const fileInput = page.locator("input[type=file]").first();
    if ((await fileInput.count()) === 0) {
      throw new Error(
        "no file input found — sign-in/room flow did not reach the composer",
      );
    }
    await fileInput.setInputFiles(filePath);

    // Wait for the transfer to finish: the rail disappears, or a generous
    // timeout elapses — whichever first, since this file is small.
    for (let i = 0; i < 200; i++) {
      const rail = page.locator(".fn-transfers");
      const visible = await rail.isVisible().catch(() => false);
      if (!visible && offsets.length > 0) break;
      await page.waitForTimeout(100);
    }
    await page.waitForTimeout(500); // let the last PATCH's request event land

    await browser.close();
    browser = null;

    if (errors.length) {
      throw new Error(
        `console/page errors during upload: ${JSON.stringify(errors.slice(0, 5))}`,
      );
    }
    if (advertisedChunkSize === null) {
      throw new Error("never saw a POST /api/uploads response with chunkSize");
    }
    if (advertisedChunkSize !== CHUNK_BYTES) {
      throw new Error(
        `server advertised chunkSize=${advertisedChunkSize}, expected ${CHUNK_BYTES} (500 KiB)`,
      );
    }
    if (offsets.length < 2) {
      throw new Error(
        `only ${offsets.length} PATCH request(s) seen — expected the ${FILE_BYTES}-byte ` +
          `file to go up in multiple ${CHUNK_BYTES}-byte pieces, not one shot`,
      );
    }

    offsets.sort((a, b) => a - b);
    // Each chunk's size is the gap to the *next* offset — the server only
    // advances `offset` by however many bytes a chunk actually contained
    // (`db/uploads.rs::advance`), so this is exact, not an estimate.
    const chunkSizes = offsets.slice(1).map((o, i) => o - offsets[i]);
    chunkSizes.push(FILE_BYTES - offsets[offsets.length - 1]); // the final chunk
    if (offsets[0] !== 0) {
      throw new Error(`first chunk landed at offset ${offsets[0]}, expected 0`);
    }
    for (let i = 0; i < chunkSizes.length; i++) {
      const isLast = i === chunkSizes.length - 1;
      const size = chunkSizes[i];
      if (!isLast && size !== CHUNK_BYTES) {
        throw new Error(
          `chunk ${i} (not the last) was ${size} bytes, expected exactly ${CHUNK_BYTES} ` +
            `(500 KiB) — chunks are fixed-size except the final short one`,
        );
      }
      if (isLast && (size <= 0 || size > CHUNK_BYTES)) {
        throw new Error(
          `final chunk was ${size} bytes, expected a positive size no larger than ${CHUNK_BYTES}`,
        );
      }
    }

    console.log(
      `OK: ${chunkSizes.length} chunks, advertised chunkSize=${advertisedChunkSize}, ` +
        `all ${CHUNK_BYTES}-byte except the final ${chunkSizes[chunkSizes.length - 1]}-byte chunk`,
    );
  } finally {
    if (browser) await browser.close().catch(() => {});
    await server.stop();
    fs.rmSync(scratchDir, { recursive: true, force: true });
  }
}

main().catch((e) => {
  console.error("FAILED:", e.message);
  process.exit(1);
});
