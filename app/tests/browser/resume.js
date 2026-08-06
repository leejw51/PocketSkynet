// Does a failed upload resume, or start over?
//
// The test kills an upload the bluntest way available — a page reload
// mid-transfer, which drops the in-flight chunk and destroys all the client's
// memory except local storage — then attaches the same file again and asks two
// questions of the network log:
//
//   1. Did the second attempt open a *new* session? (It must not.)
//   2. Did its first append start at a nonzero offset? (It must.)
//
// Both matter. Reusing the session but re-sending from zero would look like
// resume in the UI and cost the whole file.
const { chromium } = require('playwright');
const fs = require('fs');
const crypto = require('crypto');

const BIG = process.env.BIG || '/tmp/ps-big-upload.bin';
const SIZE_MB = Number(process.env.SIZE_MB || 400);
const BREAK_AT_PCT = Number(process.env.BREAK_AT_PCT || 20);

function makeFile() {
  const want = SIZE_MB * 1024 * 1024;
  if (fs.existsSync(BIG) && fs.statSync(BIG).size === want) return;
  const buf = Buffer.alloc(1024 * 1024);
  for (let i = 0; i < buf.length; i++) buf[i] = (i * 7) % 251;
  const fd = fs.openSync(BIG, 'w');
  for (let i = 0; i < SIZE_MB; i++) fs.writeSync(fd, buf);
  fs.closeSync(fd);
}

async function signInAndOpenRoom(page, ctx) {
  await page.goto('https://127.0.0.1:9099/', { waitUntil: 'domcontentloaded' });
  await page.waitForTimeout(4000);
  await page.getByRole('button', { name: /Create a wallet and sign in/i }).click();
  await page.waitForTimeout(2500);
  const copy = page.getByRole('button', { name: /Copy phrase/i }).first();
  if (await copy.isVisible().catch(() => false)) {
    await ctx.grantPermissions(['clipboard-read', 'clipboard-write']).catch(() => {});
    await copy.click();
    await page.waitForTimeout(800);
  }
  const cont = page.getByRole('button', { name: /Save the phrase to continue/i }).first();
  if (await cont.isVisible().catch(() => false)) { await cont.click(); await page.waitForTimeout(1500); }
  const signIn = page.getByRole('button', { name: /^Sign in$/i }).first();
  if (await signIn.isEnabled().catch(() => false)) await signIn.click();
  for (let i = 0; i < 40; i++) {
    if (await page.locator('input[type=file]').count() > 0) break;
    await page.keyboard.press('Escape').catch(() => {});
    await page.mouse.click(640, 400).catch(() => {});
    await page.waitForTimeout(1000);
  }
  const fast = page.getByRole('button', { name: /Fast create room/i }).first();
  if (await fast.isVisible().catch(() => false)) { await fast.click(); await page.waitForTimeout(4000); }
  if ((await page.locator('input[type=file]').count()) === 0) {
    await page.locator('[class*="fn-room-row"], li').first().click().catch(() => {});
    await page.waitForTimeout(2500);
  }
  return page.url();
}

async function main() {
  makeFile();
  const expected = crypto.createHash('sha256').update(fs.readFileSync(BIG)).digest('hex');
  console.log(`FILE ${fs.statSync(BIG).size} bytes sha256=${expected}`);

  const browser = await chromium.launch();
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true, acceptDownloads: true });
  const page = await ctx.newPage();

  const log = [];
  page.on('request', r => {
    const u = new URL(r.url());
    if (u.pathname.startsWith('/api/uploads')) {
      log.push({ phase, method: r.method(), path: u.pathname, search: u.search });
    }
  });

  let phase = 'first';
  const roomUrl = await signInAndOpenRoom(page, ctx);
  console.log('ROOM:', roomUrl);

  // --- attempt one, interrupted ---
  await page.locator('input[type=file]').first().setInputFiles(BIG);
  let broke = false;
  for (let i = 0; i < 1200; i++) {
    const txt = await page.locator('.fn-transfers').innerText().catch(() => '');
    const m = txt.replace(/\s+/g, ' ').match(/uploading\s+(\d+)%/i);
    if (m && Number(m[1]) >= BREAK_AT_PCT) {
      console.log(`INTERRUPTING at ${m[1]}% — reloading the page mid-upload`);
      broke = true;
      break;
    }
    await page.waitForTimeout(200);
  }
  if (!broke) { console.log('NEVER REACHED THE BREAK POINT'); process.exit(1); }

  const firstAppends = log.filter(e => e.phase === 'first' && e.method === 'PATCH').length;
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForTimeout(6000);

  // --- attempt two, same file ---
  phase = 'second';
  for (let i = 0; i < 40; i++) {
    if (await page.locator('input[type=file]').count() > 0) break;
    await page.keyboard.press('Escape').catch(() => {});
    await page.mouse.click(640, 400).catch(() => {});
    await page.waitForTimeout(1000);
  }
  await page.goto(roomUrl, { waitUntil: 'domcontentloaded' }).catch(() => {});
  await page.waitForTimeout(4000);
  for (let i = 0; i < 40; i++) {
    if (await page.locator('input[type=file]').count() > 0) break;
    await page.mouse.click(640, 400).catch(() => {});
    await page.waitForTimeout(1000);
  }

  await page.locator('input[type=file]').first().setInputFiles(BIG);
  console.log('RE-ATTACHED, watching...');

  // Wait for the rail to go away and *stay* away. A single invisible poll is
  // not the end of a transfer — the component re-renders between chunks — and
  // breaking on one is how the previous run closed the browser at 84%.
  let sawRail = false;
  let goneFor = 0;
  for (let i = 0; i < 4800; i++) {
    const vis = await page.locator('.fn-transfers').isVisible().catch(() => false);
    if (vis) { sawRail = true; goneFor = 0; }
    else if (sawRail && ++goneFor >= 8) break;
    await page.waitForTimeout(250);
  }
  console.log(sawRail && goneFor >= 8 ? 'transfer finished' : 'transfer did NOT finish in budget');
  await page.waitForTimeout(4000);
  await page.screenshot({ path: 'resume-done.png' });

  // --- the verdict ---
  const second = log.filter(e => e.phase === 'second');
  const newSessions = second.filter(e => e.method === 'POST' && e.path === '/api/uploads');
  const probes = second.filter(e => e.method === 'GET');
  const appends = second.filter(e => e.method === 'PATCH');
  const firstOffset = appends.length
    ? Number(new URLSearchParams(appends[0].search).get('offset'))
    : -1;

  console.log(`first attempt: ${firstAppends} chunks sent before the interruption`);
  console.log(`second attempt: ${newSessions.length} new sessions, ${probes.length} status probes, ${appends.length} chunks`);
  console.log(`second attempt first append offset = ${firstOffset}`);

  const pass =
    newSessions.length === 0 &&
    probes.length > 0 &&
    firstOffset > 0;
  console.log(pass ? 'RESUMED ✓' : 'DID NOT RESUME ✗');

  // And the result must still be the right bytes.
  try {
    const filesBtn = page.getByRole('button', { name: /files/i }).first();
    await filesBtn.click();
    await page.waitForTimeout(3000);
    const dl = page.locator('.fn-file__tools button').first();
    const [download] = await Promise.all([
      page.waitForEvent('download', { timeout: 180000 }),
      dl.click(),
    ]);
    await download.saveAs('/tmp/ps-resumed.bin');
    const got = crypto.createHash('sha256').update(fs.readFileSync('/tmp/ps-resumed.bin')).digest('hex');
    console.log(got === expected
      ? 'RESUMED FILE MATCHES ORIGINAL ✓'
      : `RESUMED FILE CORRUPT ✗ got=${got}`);
  } catch (e) {
    console.log('download check failed:', e.message);
  }

  await browser.close();
  process.exit(pass ? 0 : 1);
}

main().catch(e => { console.error('FAILED', e); process.exit(1); });
