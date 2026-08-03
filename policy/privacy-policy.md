# PocketSkynet Privacy Policy

**Effective date: August 3, 2026**

PocketSkynet is a self-hosted, end-to-end encrypted messenger, crypto wallet,
and AI agent. This policy covers the PocketSkynet iOS app.

The short version: **we run no servers and collect nothing.** The app talks
only to a PocketSkynet server that you (or someone you chose) operates, and
to services you explicitly configure.

## What we collect

Nothing. The app has no analytics, no advertising, no crash reporting, and
no tracking of any kind. The developer receives no data from the app —
there is no infrastructure on which to receive it.

## What stays on your device

- **Your recovery phrase or private key** is stored in the iOS Keychain,
  on your device. Encryption keys derived from it are held in memory only
  and are never written to disk.
- **Decrypted messages** exist only in memory. The local message cache
  stores messages exactly as they travelled the wire — encrypted.
- **Camera frames** used by the agent's optics are processed entirely
  on-device with Apple's Vision framework. No frame is uploaded anywhere.
- **The on-device AI model** (Apple Intelligence) runs locally. Using the
  agent with the on-device model sends nothing off your iPhone or iPad.

## What goes to your server

The app connects to the PocketSkynet server *you* choose on the login
screen — typically one you run yourself on your own hardware. That server
receives what the protocol requires it to relay:

- Your wallet address and signed login challenges (there are no passwords).
- Room metadata and messages. In end-to-end encrypted rooms, the server
  stores only ciphertext; it never sees plaintext or your keys.
- Your chosen username, profile image, and knowledge-base notes.

The server operator controls that data. If you run the server, you own all
of it — one SQLite file on your machine.

## What goes on-chain

Wallet transactions (sending CRO, USDC or other tokens, contract calls,
swaps) are broadcast to the Cronos blockchain and are **public and
permanent**, as with any blockchain. The app signs transactions on your
device; your private key is never transmitted.

## Third-party AI providers (optional)

The agent works with Apple's on-device model by default. If you choose to
add your own API key for an external provider (xAI Grok, OpenAI, Anthropic,
or Google Gemini), the messages you send to the agent are transmitted to
that provider under **their** privacy policy. Your key is stored on your
device and used only to call the provider you configured. Skip this and
nothing leaves the device.

## Data retention and deletion

The developer holds no data about you, so there is nothing for us to retain
or delete. To remove data:

- **On your device** — delete the app; the Keychain entry is removed.
- **On your server** — the operator can delete the SQLite database, or you
  can use the in-app tools (e.g. forgetting knowledge notes).
- **On-chain** — blockchain records are permanent by design and cannot be
  deleted by anyone.

## Children

PocketSkynet is not directed at children and, as a cryptocurrency wallet,
is not appropriate for users under 17.

## Changes

Changes to this policy are published at this URL with an updated effective
date, and its history is visible in this repository's git log.

## Contact

Questions about this policy: **leejw51@gmail.com**, or open an issue at
<https://github.com/leejw51/PocketSkynet>.
