// End-to-end: sign in, make a room, attach a file far larger than the old
// 25 MB cap, and catch the progress rail while it is on screen.
const { chromium } = require('playwright');
const fs = require('fs');
const crypto = require('crypto');

const BIG = process.env.BIG || '/tmp/ps-big-upload.bin';
const SIZE_MB = Number(process.env.SIZE_MB || 120);

async function main() {
  if (!fs.existsSync(BIG) || fs.statSync(BIG).size !== SIZE_MB * 1024 * 1024) {
    const buf = Buffer.alloc(1024 * 1024);
    for (let i = 0; i < buf.length; i++) buf[i] = i % 251;
    const fd = fs.openSync(BIG, 'w');
    for (let i = 0; i < SIZE_MB; i++) fs.writeSync(fd, buf);
    fs.closeSync(fd);
  }
  const expected = crypto
    .createHash('sha256')
    .update(fs.readFileSync(BIG))
    .digest('hex');
  console.log(`FILE ${BIG} ${fs.statSync(BIG).size} bytes sha256=${expected}`);

  const browser = await chromium.launch();
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true, acceptDownloads: true });
  const page = await ctx.newPage();
  const errors = [];
  page.on('console', m => { if (m.type() === 'error') errors.push(m.text()); });
  page.on('pageerror', e => errors.push('pageerror: ' + e.message));

  // Watch the upload protocol actually being spoken.
  const calls = [];
  page.on('response', async r => {
    const u = new URL(r.url()).pathname + new URL(r.url()).search;
    if (u.includes('/api/uploads') || u.includes('/download-token') || u.includes('/raw')) {
      calls.push(`${r.request().method()} ${u} -> ${r.status()}`);
    }
  });

  await page.goto('https://127.0.0.1:9099/', { waitUntil: 'domcontentloaded' });
  await page.waitForTimeout(4000);

  // 1. Sign in with a fresh wallet.
  await page.getByRole('button', { name: /Create a wallet and sign in/i }).click();
  await page.waitForTimeout(2500);

  // The phrase gate: the app refuses to continue until the phrase has been
  // copied or downloaded, which is the whole point of that screen.
  const copy = page.getByRole('button', { name: /Copy phrase/i }).first();
  if (await copy.isVisible().catch(() => false)) {
    await ctx.grantPermissions(['clipboard-read', 'clipboard-write']).catch(() => {});
    await copy.click();
    await page.waitForTimeout(800);
  }
  const cont = page.getByRole('button', { name: /Save the phrase to continue/i }).first();
  if (await cont.isVisible().catch(() => false)) {
    await cont.click();
    await page.waitForTimeout(1500);
  }
  await page.screenshot({ path: 'after-phrase.png' });

  // A username, if it asks.
  const uname = page.locator('input[type=text]').first();
  if (await uname.isVisible().catch(() => false)) {
    const v = await uname.inputValue().catch(() => '');
    if (!v) { await uname.fill('bigfiles'); await page.waitForTimeout(400); }
  }
  const signIn = page.getByRole('button', { name: /^Sign in$/i }).first();
  if (await signIn.isEnabled().catch(() => false)) {
    await signIn.click();
  }

  // The boot cutscene sits between sign-in and the app; click through it.
  for (let i = 0; i < 40; i++) {
    if (await page.locator('input[type=file]').first().count() > 0) break;
    await page.keyboard.press('Escape').catch(() => {});
    await page.mouse.click(640, 400).catch(() => {});
    await page.waitForTimeout(1000);
  }
  await page.screenshot({ path: 'after-signin.png' });
  console.log('AFTER SIGNIN URL:', page.url());

  // 2. A room to attach into. "Fast create room" is one click and no dialog.
  const fast = page.getByRole('button', { name: /Fast create room/i }).first();
  if (await fast.isVisible().catch(() => false)) {
    await fast.click();
    await page.waitForTimeout(4000);
  }
  await page.screenshot({ path: 'after-room.png' });

  // Open the first room in the list if we are not already inside one.
  if ((await page.locator('input[type=file]').count()) === 0) {
    const row = page.locator('[class*="fn-room-row"], li').first();
    if (await row.isVisible().catch(() => false)) {
      await row.click();
      await page.waitForTimeout(2500);
    }
  }
  await page.screenshot({ path: 'in-room.png' });
  console.log('IN ROOM URL:', page.url(), 'file inputs:', await page.locator('input[type=file]').count());

  // 3. Attach. The composer's picker is a hidden input; set it directly.
  const fileInput = page.locator('input[type=file]').first();
  await fileInput.setInputFiles(BIG);
  console.log('ATTACHED, watching for the progress rail...');

  // 4. Catch the rail while it exists.
  let sawRail = false;
  let stages = new Set();
  let maxPct = 0;
  for (let i = 0; i < 800; i++) {
    const rail = page.locator('.fn-transfers');
    if (await rail.isVisible().catch(() => false)) {
      sawRail = true;
      const txt = (await rail.innerText().catch(() => '')).replace(/\s+/g, ' ');
      const m = txt.match(/(\d+)%/);
      if (m) maxPct = Math.max(maxPct, Number(m[1]));
      const stage = txt.match(/checking|uploading|verifying/i);
      if (stage) stages.add(stage[0].toLowerCase());
      if (!sawRailLogged.has(txt)) { console.log('RAIL:', txt); sawRailLogged.add(txt); }
      if (!shot && /uploading/i.test(txt)) {
        await page.screenshot({ path: 'progress.png' });
        shot = true;
      }
    } else if (sawRail) {
      break; // it finished
    }
    await page.waitForTimeout(250);
  }
  console.log(`RAIL SEEN=${sawRail} stages=${[...stages]} maxPct=${maxPct}`);

  await page.waitForTimeout(4000);
  await page.screenshot({ path: 'after-upload.png' });

  // 5. Download it back through the capability URL, and check the bytes.
  try {
    const filesBtn = page.getByRole('button', { name: /files/i }).first();
    if (await filesBtn.isVisible().catch(() => false)) {
      await filesBtn.click();
      await page.waitForTimeout(2500);
      await page.screenshot({ path: 'files-drawer.png' });
      const dl = page.locator('.fn-file__tools button').first();
      if (await dl.isVisible().catch(() => false)) {
        const [download] = await Promise.all([
          page.waitForEvent('download', { timeout: 120000 }),
          dl.click(),
        ]);
        const to = '/tmp/ps-downloaded.bin';
        await download.saveAs(to);
        const got = crypto.createHash('sha256').update(fs.readFileSync(to)).digest('hex');
        console.log(`DOWNLOAD ${fs.statSync(to).size} bytes sha256=${got}`);
        console.log(got === expected ? 'DOWNLOAD MATCHES ORIGINAL' : 'DOWNLOAD MISMATCH');
      } else {
        console.log('no download button found in the drawer');
      }
    }
  } catch (e) {
    console.log('DOWNLOAD STEP FAILED:', e.message);
  }
  await page.screenshot({ path: 'after-download.png' });

  console.log('UPLOAD CALLS:');
  const seen = new Map();
  for (const c of calls) seen.set(c.replace(/offset=\d+/, 'offset=N'), (seen.get(c.replace(/offset=\d+/, 'offset=N')) || 0) + 1);
  for (const [k, v] of seen) console.log(`  ${v}x ${k}`);
  console.log('CONSOLE ERRORS:', JSON.stringify(errors.slice(0, 10)));

  await browser.close();
}

const sawRailLogged = new Set();
let shot = false;
main().catch(e => { console.error('FAILED', e); process.exit(1); });
