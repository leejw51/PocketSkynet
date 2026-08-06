// Sign in to a running PocketSkynet server the way a real client does:
// ask for a challenge, sign it with the wallet key (EIP-191), exchange the
// signature for a JWT. No shortcuts — this is the same flow the browser runs,
// so anything these tests prove is true of the deployed server.

const { Wallet, id: keccakOfString } = require('ethers');

const BASE = process.env.PS_BASE || 'http://127.0.0.1:9399';

// This suite must not reach a server anybody cares about. Two reasons, both
// hard: the wallets below have PUBLIC private keys, and the admin spec is
// destructive by design — it suspends accounts, evicts them from every room,
// and deletes rooms. Against the throwaway server run.sh starts, that is the
// test; against a real deployment it is an incident. The guard is here rather
// than in run.sh because `npx playwright test` bypasses run.sh.
{
  const host = new URL(BASE).hostname;
  const local = ['127.0.0.1', 'localhost', '::1', '[::1]'].includes(host);
  if (!local && process.env.PS_ALLOW_REMOTE !== '1') {
    throw new Error(
      `PS_BASE points at ${host}, which is not local. This suite signs in with ` +
        'publicly-known private keys and destructively exercises the admin API. ' +
        'If you truly mean to run it against a remote server, set PS_ALLOW_REMOTE=1.',
    );
  }
}

/// The one wallet this server is configured to treat as an administrator.
/// Fixed, because it has to match the `VITE_FRUITNATION_ADMIN` that `run.sh`
/// sets — that is the whole point of the role.
///
/// ⚠️  This private key is PUBLIC — it is in the repository, like Hardhat's
/// junk mnemonic or Anvil's well-known keys, and the labelled wallets below
/// are brainwallets (keccak256 of a string in this file). Three consequences:
///
///   * NEVER send funds to any of these addresses. A funded known-key
///     address is swept by bots within seconds.
///   * NEVER copy this wallet's address into a real deployment's
///     VITE_FRUITNATION_ADMIN — that hands the server's admin role to
///     anyone who can read this file.
///   * NEVER aim this suite at a server you care about (the guard above
///     enforces this one).
const BOSS_KEY = '0x2db142be06a0c1c779b8f0d65640ceb43de9af1a36552374ad2ac965bdc46e1e';
const BOSS_ADDRESS = '0xac550F3DA533F335f33ED7a316b2D361DF03919F';

/// Everybody else is derived from their label.
///
/// Deterministic so a failure is reproducible, and *per-label* so no two tests
/// share a participant. That matters more than it looks: an early version used
/// one "carol" across every spec, and the admin test that suspends her left
/// every later test signing in as a suspended account. Tests that share mutable
/// identities do not fail independently, and a cascade hides which assertion
/// actually broke.
function walletFor(label) {
  if (label === 'boss') return new Wallet(BOSS_KEY);
  return new Wallet(keccakOfString(`pocketskynet-e2e/${label}`));
}

const tokens = new Map();

/// Sign in as `label`, reusing the token for the rest of the run.
///
/// Reuse is not only an optimisation: the challenge endpoint is rate limited
/// per IP, and a suite that re-authenticates in every test ends up testing the
/// limiter. A real client signs in once and keeps its token.
async function signIn(request, label, { fresh = false } = {}) {
  if (!fresh && tokens.has(label)) return tokens.get(label);
  const session = await authenticate(request, label);
  tokens.set(label, session);
  return session;
}

function forget(label) {
  tokens.delete(label);
}

async function authenticate(request, label) {
  const wallet = walletFor(label);
  // Usernames must be 3–100 characters, and a mention resolves by username, so
  // the label doubles as the handle a test can write as `@label`.
  const username = label.length >= 3 ? label : `${label}-user`;

  const challenge = await request.post(`${BASE}/api/auth/challenge`, {
    data: { walletAddress: wallet.address },
  });
  if (!challenge.ok()) {
    throw new Error(`challenge ${challenge.status()}: ${await challenge.text()}`);
  }
  const { challengeId, message } = await challenge.json();

  const signature = await wallet.signMessage(message);
  const login = await request.post(`${BASE}/api/auth/login`, {
    data: { walletAddress: wallet.address, signature, challengeId, username },
  });
  if (!login.ok()) throw new Error(`login ${login.status()}: ${await login.text()}`);
  const body = await login.json();

  return {
    label,
    username,
    // The server's own spelling, not ethers'. `WalletAddress` normalises to
    // lowercase on the way in ("accepts any casing, stores lowercase"), so
    // this is the string every response will contain — comparing against the
    // checksummed form tests the casing rule rather than the feature.
    address: body.user.walletAddress,
    // What a person would paste, to prove the server accepts it and answers
    // in its own casing.
    checksummed: wallet.address,
    token: body.token,
    isServerAdmin: body.isServerAdmin,
    auth: { Authorization: `Bearer ${body.token}` },
  };
}

/** A thin wrapper so tests read as intent, not as fetch plumbing. */
function api(request, user) {
  const headers = user ? user.auth : {};
  return {
    get: (path) => request.get(`${BASE}${path}`, { headers }),
    post: (path, data) => request.post(`${BASE}${path}`, { headers, data: data ?? {} }),
    patch: (path, data) => request.patch(`${BASE}${path}`, { headers, data: data ?? {} }),
    delete: (path) => request.delete(`${BASE}${path}`, { headers }),
  };
}

const hash = (seed) => String(seed).repeat(64).slice(0, 64);

async function json(response) {
  const text = await response.text();
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

/** Create a channel and return its id. */
async function channel(request, user, name) {
  return (await json(await api(request, user).post('/api/rooms', { name }))).id;
}

/** Open (or find) the DM between two people and return its id. */
async function dm(request, from, to) {
  return (await json(await api(request, from).post('/api/rooms/dm', { walletAddress: to.address })))
    .id;
}

/** Post a message and return it. */
async function post(request, user, room, content, extra = {}) {
  return json(
    await api(request, user).post(`/api/rooms/${room}/messages`, {
      content,
      msgHash: hash('a'),
      ...extra,
    }),
  );
}

/** Put `who` into `room` through the real invite/accept handshake. */
async function join(request, inviter, who, room) {
  await api(request, inviter).post(`/api/rooms/${room}/invite`, { userAddress: who.address });
  await api(request, who).post(`/api/invitations/${room}/accept`);
}

module.exports = {
  BASE,
  BOSS_ADDRESS,
  signIn,
  forget,
  walletFor,
  api,
  hash,
  json,
  channel,
  dm,
  post,
  join,
};
