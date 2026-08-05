# Browser tests for large file transfers

Two things about a 4 GB upload cannot be tested anywhere but a real browser, and
both were found by these scripts rather than by review:

* **The client must never hold the file.** A `Vec<u8>` of the attachment is one
  line away at all times, and nothing in the Rust type system objects — the
  wasm heap simply runs out at a size no unit test uses. Only a browser moving
  a real 120 MB file shows it.
* **Resume has to survive the client's memory being destroyed.** The interesting
  failure is not a dropped chunk, it is a reload: the tab loses everything
  except local storage, and the question is whether the *next* attempt finds the
  half-finished session or quietly starts a second one.

The server's own suite (`server/tests/uploads.rs`) covers the protocol —
offsets, digests, ranges, capabilities — with small payloads, because those are
boundary properties and none of them get truer at 4 GB. These two cover what
that suite structurally cannot: the browser.

## Running them

They drive a **running server** rather than starting one, because they are
checking the thing that was actually built:

```sh
make restart          # from app/
cd tests/browser
npm install           # playwright
npx playwright install chromium
node upload.js        # round trip, progress, digest
node resume.js        # interrupted transfer, then resume
```

Both accept `SIZE_MB` (default 120) and `BIG` (the scratch file path);
`resume.js` also takes `BREAK_AT_PCT`, the point at which it reloads the page
mid-transfer.

`resume.js` exits non-zero when the second attempt fails to resume, so it works
as a check. `upload.js` reports rather than asserts — its value is the network
log it prints, which is what tells you whether a change quietly went back to
sending the file in one request.

## What they assert

`upload.js`

* the progress rail appears, names both passes (`CHECKING`, then `UPLOADING`),
  and reaches 100%
* the wire carries `POST /api/uploads` once, `PATCH …?offset=` per chunk, and
  one `finish` — not a single large body
* the file downloads back through a capability URL with **the same sha-256**
* no console errors anywhere in the run

`resume.js`

* an upload interrupted by a page reload does **not** open a second session
* its first chunk after the interruption goes to a **nonzero offset** — reusing
  the session but re-sending from zero would look identical in the UI and cost
  the whole file, so the offset is checked, not just the session id
* the resumed file's sha-256 still matches the original

## Notes

* HTTPS with a generated CA, so both pass `ignoreHTTPSErrors`.
* Sign-in creates a throwaway wallet each run. The phrase gate is real — the app
  will not continue until the phrase has been copied — so the scripts click
  through it rather than around it.
* They leave their scratch files in `/tmp` and their screenshots in the working
  directory; neither is committed (see the repo `.gitignore`).

## `ignoreHTTPSErrors` hides a real bug — know what it costs you

Both scripts pass `ignoreHTTPSErrors`, because the server generates its own CA
and a test that stopped at a certificate warning would test nothing. That flag
also conceals the single hardest bug found in this work, so it is worth writing
down what it hides.

**iOS plays `<video>` in a different process from the page.** That media process
does not inherit a certificate exception you accepted in Safari. So against a
server with an untrusted self-signed certificate, on an iPhone:

* the page loads — you tapped through the warning;
* `fetch` works, so metadata, listings and the download button are all fine;
* `<video>` fails **silently**, showing `--:--` forever and reporting nothing.

Nothing in the app is wrong, no console error appears, and every desktop browser
plays it perfectly — because `ignoreHTTPSErrors` puts the test on the other side
of exactly the trust decision that is failing.

Four rounds of code changes went looking for this in the wrong place. What
found it was running the server with `make restart TLS=0 HTTP3=0` and loading it
over plain `http://` from the phone, where it played immediately.

The fix is not in the code. Install the server's CA on the device:
`https://<server>/ca.crt`, then **Settings → General → VPN & Device
Management** to install the profile, then **Settings → General → About →
Certificate Trust Settings** to grant it full trust. HTTP/3 needs this too —
QUIC has no plaintext mode, so an untrusted certificate takes it out entirely.

If a media element ever "does nothing" on a device again, check the padlock
before reading any Rust.
