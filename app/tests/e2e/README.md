# End-to-end tests: DMs, threads, mentions, administration

Forty checks that run against a **real server process over real HTTP**,
signing in with real wallet signatures. They exist because the Rust suite,
thorough as it is, cannot answer three questions:

* **Is the role really configuration?** `VITE_FRUITNATION_ADMIN` is read from
  the environment of a running process. A unit test that sets the variable
  in-process proves the parser works, not that a deployment configured this way
  produces an administrator. `run.sh` starts a server with the variable set to
  one wallet and the suite signs in as that wallet.
* **Does the client survive the new wire shape?** Rooms grew `kind`, messages
  grew `parentMessageId` / `replyCount` / `lastReplyAt`, and the room list grew
  `mentionCount`. The WASM client is compiled separately and is not exercised
  by `cargo test` at all. `browser.spec.js` drives it in Chromium and fails on
  any console error.
* **Does sign-in actually work end to end?** Challenge → EIP-191 signature →
  JWT, through the same endpoints a browser uses, with a real secp256k1 key.

## Running them

```sh
cd app
cargo build -p pocketskynet-server     # run.sh uses target/debug
make web                               # browser.spec.js needs web/dist

cd tests/e2e
npm install
npx playwright install chromium
./run.sh                               # everything
./run.sh admin.spec.js                 # one file
./run.sh -g "thread"                   # one pattern
```

`run.sh` starts its own server on port 9401 with a fresh data directory and
kills it afterwards. It **refuses to run if that port is already busy**, which
is not defensive padding: an earlier version shared a port with a
hand-started server, the health check was answered by that other process, and
the suite silently tested a database it had not prepared.

## Why the tests look the way they do

**Every test gets its own participants.** Wallets are derived from a label
(`helpers.js`, `walletFor`), so `mn-inbox-bob` and `ad-susp-carol` are
different people. The first version shared one "carol" across all four files;
the admin test suspends her, and every later test then signed in as a suspended
account. Tests that share mutable identities do not fail independently, and the
resulting cascade hides which assertion actually broke.

**Tokens are cached for the run.** The challenge endpoint is rate limited per
IP, and re-authenticating in every test measures the limiter rather than the
feature. A real client signs in once and keeps its token. `run.sh` additionally
passes `--no-rate-limit`, which documents itself as existing for exactly this
case.

**The wallet keys are public, and the suite refuses remote servers.** The
`boss` key is a committed literal and every labelled wallet is a brainwallet
(`keccak256("pocketskynet-e2e/<label>")`) — the same convention as Hardhat's
junk mnemonic. Never fund these addresses, and never reuse the boss address in
a real `VITE_FRUITNATION_ADMIN`. Because the admin spec is destructive
(suspends, evicts, deletes rooms), `helpers.js` throws if `PS_BASE` is not
localhost unless `PS_ALLOW_REMOTE=1` is set explicitly.

**Addresses come from the server, not from ethers.** `WalletAddress` normalises
to lowercase on the way in. Comparing against the checksummed form tests the
casing rule instead of the feature — though one test deliberately *sends* the
checksummed form, to prove a DM key cannot fork on casing.

## What `browser.spec.js` covers

The whole client half of step 1, through the interface a person uses: signing
in, opening a DM from the picker, collapsing and expanding a thread, replying
into one from the composer, completing an `@mention` with the keyboard, the
mentions inbox, and the admin console appearing for an administrator and not
for anybody else. Every one of them fails on a console error, which is how a
wire-shape regression in the WASM client would surface.

Two assertions are worth knowing about because they look wrong and are not:

* **A thread reply is `toHaveCount(0)` in the channel log but present after
  expanding.** `/sync` delivers replies — it has to, or an offline client would
  lose them — so the client holds every reply and simply does not draw it in
  the channel. That is what makes opening a thread instant and offline.
* **The room-list preview *does* quote a thread reply.** Deliberate: a room
  whose only recent activity is in a thread should not look idle. So the
  channel-view assertions are scoped to `getByRole('log')`, or they would match
  the sidebar and prove nothing.
