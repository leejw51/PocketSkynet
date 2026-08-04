// The service worker: what makes the client installable and what makes it open
// without a network round trip.
//
// It caches the *shell* — `index.html`, the WASM bundle, the stylesheet, the
// boot imagery — and nothing else. No message, no room key and no API response
// is ever written to Cache Storage. That is deliberate and it is the same rule
// the client already follows in memory (README, "Storage"): the server's data
// belongs to a signed-in session, and a cache that survives sign-out would
// quietly outlive it.
//
// Two regimes, mirroring the two the server already serves with (`routes/mod.rs`,
// `cache_control`):
//
// - **Content-hashed** files — `app-<hash>.css`, `<crate>-<hash>.js`,
//   `<crate>-<hash>_bg.wasm`. A changed file is a changed URL, so a hit is
//   always correct and is answered with no network request at all.
// - **Stable URLs** — `/static/img/…`, the vendored Topcoat sheet. Answered
//   from the cache and revalidated in the background, so an edited asset is
//   picked up on the next load.
//
// The document itself is network-first, not cache-first, and that is the one
// place a slower answer is bought on purpose. The server is usually on the LAN
// or on loopback, so the network costs milliseconds; being one launch behind
// after a redeploy costs a confusing bug report. Offline, the cached shell is
// served instead, which is why a home-screen launch with no server reachable
// still paints the app rather than the browser's dinosaur.

const SHELL = '/';
const SHELL_CACHE = 'pocketskynet-shell-v1';
const ASSET_CACHE = 'pocketskynet-assets-v1';
const CACHES = [SHELL_CACHE, ASSET_CACHE];

/// Files the shell needs but does not name.
///
/// The typefaces are reached from `@font-face` inside the stylesheet, and the
/// manifest icons from the manifest, so neither appears in the HTML the asset
/// list is read out of. Without the fonts a first offline launch renders in a
/// fallback serif; without an icon the install prompt has nothing to show.
///
/// Only the 192s: the 512s are four times the bytes and are wanted in exactly
/// one place, the install prompt, which is a thing that happens online. Once
/// installed, the launcher draws from the copy the OS took at install time and
/// never asks for this cache at all.
///
/// The line is drawn here on purpose: `static/img/` is 26 MB of generated
/// artwork, and precaching it would turn a 3 MB install into a download nobody
/// asked for. The rest of the imagery is cached as it is actually used.
const ALWAYS = [
  '/static/fonts/chakra-petch-600-latin.woff2',
  '/static/fonts/chakra-petch-700-latin.woff2',
  '/static/img/icon-192.png',
  '/static/img/icon-maskable-192.png',
];

/// Paths that must always reach the server.
///
/// `/api/events` is SSE and `/ws` is a socket — neither is a document that can
/// be replayed, and a cached one would be a stream that never moves. Published
/// sites (`/sites/…`) are other people's pages served under their own sandbox
/// and have no business in this app's cache; `/ca.crt` is fetched exactly once,
/// by a device that is trying to trust the server, and must never be an old copy.
function isLive(url) {
  const path = url.pathname;
  return (
    path.startsWith('/api/') ||
    path === '/ws' ||
    path === '/sites' ||
    path.startsWith('/sites/') ||
    path === '/ca.crt'
  );
}

/// Whether a path carries a content hash, and so can never change meaning.
///
/// The rule is the server's `is_content_hashed`, plus `.css` — Trunk hashes the
/// stylesheet too, and here that is worth acting on. An unrecognised name falls
/// through to revalidate-in-background, which is merely slower, never wrong.
function isHashed(url) {
  const file = url.pathname.split('/').pop() || '';
  const stem = file.endsWith('_bg.wasm')
    ? file.slice(0, -'_bg.wasm'.length)
    : /\.(js|wasm|css)$/.test(file)
      ? file.slice(0, file.lastIndexOf('.'))
      : null;
  if (stem === null) return false;

  const dash = stem.lastIndexOf('-');
  if (dash === -1) return false;
  const tail = stem.slice(dash + 1);
  return tail.length >= 8 && /^[0-9a-f]+$/i.test(tail);
}

/// Every root-absolute URL the shell names.
///
/// The built `index.html` is the only thing that knows the current hashed
/// filenames — Trunk writes them into the `modulepreload` and `preload` links
/// and into the module script's import. Reading them back out of the document
/// means this worker never has to be regenerated when the bundle changes, and
/// never has a list to fall out of date.
function referencedAssets(html) {
  const urls = new Set();
  for (const [, value] of html.matchAll(/(?:src|href)=["']([^"']+)["']/g)) {
    if (value.startsWith('/')) urls.add(value);
  }
  // The module script imports the JS glue rather than linking it, so it is
  // reached by a bare `from '/…js'` with no attribute around it.
  for (const [, value] of html.matchAll(/from\s+'(\/[^']+)'/g)) {
    urls.add(value);
  }
  for (const [, value] of html.matchAll(/module_or_path:\s*'(\/[^']+)'/g)) {
    urls.add(value);
  }
  return [...urls];
}

/// Fetch the shell and everything it names, so the very first offline launch —
/// the one right after "Add to Home Screen" — already works.
///
/// Nothing here may reject: a failed install leaves the page with no worker at
/// all, and one missing image is not worth that. Whatever did not arrive is
/// fetched and cached on the next load anyway.
async function precache() {
  try {
    const response = await fetch(SHELL, { cache: 'reload' });
    if (!response.ok) return;
    const html = await response.clone().text();
    const cache = await caches.open(SHELL_CACHE);
    await cache.put(SHELL, response);
    await cacheAssets(referencedAssets(html));
  } catch {
    // Offline at install time. The next successful load fills the cache.
  }
}

/// Store the named assets, and drop the hashed ones the shell no longer names.
///
/// The pruning is the point. Each redeploy mints a new `_bg.wasm` — 2.5 MB —
/// under a new URL, and without this the cache would grow by that much every
/// time, forever, on a device nobody thinks to clear. The shell is the manifest
/// of what is current: a hashed file it does not mention is unreachable.
async function cacheAssets(referenced) {
  const wanted = [...new Set([...referenced, ...ALWAYS])];
  const cache = await caches.open(ASSET_CACHE);
  await Promise.allSettled(
    wanted.map((url) => cache.add(new Request(url, { cache: 'reload' }))),
  );

  const keep = new Set(wanted.map((url) => new URL(url, self.location.origin).href));
  for (const request of await cache.keys()) {
    if (isHashed(new URL(request.url)) && !keep.has(request.url)) {
      await cache.delete(request);
    }
  }
}

/// Adopt a freshly fetched document as the shell.
///
/// Only a changed document triggers the asset pass: the common case is an
/// unchanged shell on every navigation, and re-adding a 2.5 MB bundle each time
/// would make the worker cost more than it saves.
async function adoptShell(response) {
  const html = await response.clone().text();
  const cache = await caches.open(SHELL_CACHE);
  const previous = await cache.match(SHELL);
  const changed = !previous || (await previous.text()) !== html;

  await cache.put(SHELL, response);
  if (changed) await cacheAssets(referencedAssets(html));
}

/// Documents: network first, cached shell when the network is not there.
///
/// Deep links are stored under `/` on purpose. The server answers every client
/// route with the same `index.html` (the SPA fallback), so `/rooms/abc` and `/`
/// are the same bytes, and keeping one entry means an offline deep link is
/// served the shell that then routes itself.
async function serveDocument(event) {
  try {
    const fresh = await fetch(event.request);
    if (fresh.ok && fresh.headers.get('content-type')?.startsWith('text/html')) {
      event.waitUntil(adoptShell(fresh.clone()));
    }
    return fresh;
  } catch (error) {
    const cache = await caches.open(SHELL_CACHE);
    const shell = await cache.match(SHELL);
    if (shell) return shell;
    throw error;
  }
}

/// Assets: a hashed hit is final, anything else is served then revalidated.
async function serveAsset(event) {
  const cache = await caches.open(ASSET_CACHE);
  const hit = await cache.match(event.request);
  if (hit && isHashed(new URL(event.request.url))) return hit;

  const network = fetch(event.request).then((response) => {
    // 200 only: `cache.put` rejects on a 206, and caching a redirect or an
    // error would pin the failure.
    if (response.status === 200) {
      cache.put(event.request, response.clone()).catch(() => {});
    }
    return response;
  });

  if (hit) {
    event.waitUntil(network.catch(() => {}));
    return hit;
  }
  return network;
}

self.addEventListener('install', (event) => {
  // `skipWaiting` is safe here in a way it is not for most apps: every asset
  // this worker caches is content-hashed or revalidated, so a new worker taking
  // over a live page cannot hand it a stale bundle.
  event.waitUntil(precache().then(() => self.skipWaiting()));
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    (async () => {
      const names = await caches.keys();
      await Promise.all(
        names.filter((name) => !CACHES.includes(name)).map((name) => caches.delete(name)),
      );
      await self.clients.claim();
    })(),
  );
});

self.addEventListener('fetch', (event) => {
  const request = event.request;
  if (request.method !== 'GET') return;

  const url = new URL(request.url);
  // Cross-origin is left entirely alone: the wallet and AI providers the client
  // talks to are not ours to cache, and an opaque response tells us nothing
  // about whether it succeeded.
  if (url.origin !== self.location.origin) return;
  if (isLive(url)) return;

  event.respondWith(request.mode === 'navigate' ? serveDocument(event) : serveAsset(event));
});
