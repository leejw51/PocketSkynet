const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: __dirname,
  // Serial. These specs share one server and one set of wallets, so a room
  // Alice opens in one file is visible to another — running them in parallel
  // would make "how many rooms does Alice have" a race rather than a fact.
  workers: 1,
  fullyParallel: false,
  reporter: [['list']],
  timeout: 30_000,
  use: {
    baseURL: process.env.PS_BASE || 'http://127.0.0.1:9399',
    trace: 'off',
    // 127.0.0.1 counts as a secure context, so `crypto.subtle` — and
    // therefore wallet sign-in — works over plain HTTP here. A LAN address
    // would not; see the Makefile's note on why HTTPS is the default.
    headless: true,
  },
});
