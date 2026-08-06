// Paste this whole thing into the browser console, on the chat screen, then
// send a message. It reports what the page is actually doing rather than what
// the code intends — which is the gap every fix so far has fallen into.
//
//   Safari: Develop ▸ Show JavaScript Console  (enable Develop in Settings ▸ Advanced)
//   Chrome: View ▸ Developer ▸ JavaScript Console
(() => {
  const log = document.querySelector('[role=log]');
  if (!log) return console.log('PS-DIAG: no [role=log] on screen — open a room first');

  // 1. Which bundle is running? If this hash is not the newest build, the
  //    browser is serving cached code and no fix can have reached it.
  const wasm = performance.getEntriesByType('resource')
    .map((e) => e.name).filter((n) => n.endsWith('.wasm'));
  console.log('PS-DIAG bundle :', wasm.join(', ') || '(not seen in this navigation)');

  // 2. Which element actually scrolls? If the *document* scrolls rather than
  //    the stream, scrolling the stream to its end still leaves the newest
  //    message off-screen — and that would look exactly like "not scrolling".
  const docScrolls =
    document.scrollingElement.scrollHeight > document.scrollingElement.clientHeight + 4;
  console.log('PS-DIAG doc scrolls?', docScrolls,
    'docScrollTop=', document.scrollingElement.scrollTop);

  const geo = () => ({
    d: Math.round(log.scrollHeight - log.scrollTop - log.clientHeight),
    top: Math.round(log.scrollTop),
    h: log.scrollHeight,
    c: log.clientHeight,
    pageY: Math.round(window.scrollY),
  });
  console.log('PS-DIAG start  :', JSON.stringify(geo()));

  // 3. Did the stream grow without the view following?
  let last = JSON.stringify(geo());
  const timer = setInterval(() => {
    const now = JSON.stringify(geo());
    if (now !== last) { console.log('PS-DIAG change :', now); last = now; }
  }, 300);

  // 4. Are the load events the re-settle depends on actually firing here?
  const onLoad = (e) =>
    console.log('PS-DIAG media  :', e.type, e.target.tagName,
      String(e.target.currentSrc || e.target.src || '').slice(-40));
  log.addEventListener('load', onLoad, true);
  log.addEventListener('loadedmetadata', onLoad, true);

  setTimeout(() => {
    clearInterval(timer);
    log.removeEventListener('load', onLoad, true);
    log.removeEventListener('loadedmetadata', onLoad, true);
    console.log('PS-DIAG done   :', JSON.stringify(geo()));
  }, 45000);

  console.log('PS-DIAG watching for 45s — now send a message.');
})();
