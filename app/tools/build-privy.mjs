#!/usr/bin/env node
// Bundle the Privy bridge (web/privy/bridge.jsx) into a single checked-in file.
//
// Run it with `make privy`. You only need to run it when the bridge changes or
// Privy is upgraded — the output is committed, exactly like the vendored Topcoat
// CSS, because **nothing in this app may be fetched at runtime**. An air-gapped
// LAN deployment and a packaged desktop binary both have to work with no network
// and no npm.
//
// Resolution is the one awkward part. Privy, React and esbuild all live in the
// *reference client's* `node_modules` (`server/node_modules`), which is a git
// submodule; this repository deliberately has no `package.json` of its own and
// no npm install step. So esbuild is required by absolute path and pointed at
// that directory via `nodePaths`, rather than the bridge being moved into the
// submodule where it does not belong.
//
// If `server/node_modules` is absent (a fresh clone that has not run the
// reference client's install), this exits with a clear message and changes
// nothing. The committed bundle keeps working; only *rebuilding* needs the deps.

import { existsSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..");
const refModules = resolve(root, "..", "server", "node_modules");

if (!existsSync(refModules)) {
  console.error(
    `Cannot rebuild the Privy bundle: ${refModules} does not exist.\n` +
      `It supplies @privy-io/react-auth, react and esbuild. Run the reference\n` +
      `client's install first (cd ../server && pnpm install).\n\n` +
      `The committed bundle at web/static/vendor/privy/privy.js is unaffected.`,
  );
  process.exit(1);
}

const require = createRequire(join(refModules, "noop.js"));
let esbuild;
try {
  esbuild = require("esbuild");
} catch {
  console.error(`esbuild not found under ${refModules}.`);
  process.exit(1);
}

const outfile = resolve(root, "web", "static", "vendor", "privy", "privy.js");
await mkdir(dirname(outfile), { recursive: true });

const result = await esbuild.build({
  entryPoints: [resolve(root, "web", "privy", "bridge.jsx")],
  outfile,
  bundle: true,
  minify: true,
  // A classic script, not a module: index.html loads it with a plain <script>
  // so it has executed and defined `window.psPrivy` before the WASM starts.
  // An ESM build would defer, and the Rust side would race it.
  format: "iife",
  platform: "browser",
  target: ["es2020"],
  // React reads this to drop its development-only warnings and checks.
  define: { "process.env.NODE_ENV": '"production"' },
  jsx: "automatic",
  // The bridge lives outside the tree that owns these packages.
  nodePaths: [refModules],
  legalComments: "none",
  logLevel: "warning",
  metafile: true,
});

const bytes = Object.values(result.metafile.outputs)[0].bytes;
console.log(
  `privy.js  ${(bytes / 1024).toFixed(0)} KB  (React + react-dom + @privy-io/react-auth)`,
);
console.log(`written to web/static/vendor/privy/privy.js — commit it`);
