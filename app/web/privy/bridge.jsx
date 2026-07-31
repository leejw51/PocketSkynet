// The Privy bridge.
//
// Privy ships as a React SDK — `usePrivy`, `useWallets`, `useCreateWallet` are
// hooks, so they only exist inside a React tree. This client is Yew/WASM and has
// no React. So this file is the smallest possible React application whose entire
// job is to turn those hooks into an imperative, promise-based API that
// `wasm-bindgen` can call.
//
// **The shape of the bridge is the important decision.** It does not expose
// "login with Privy" as a feature. It exposes an **EIP-1193 provider**, obtained
// from `wallet.getEthereumProvider()`. That means the Rust side has exactly one
// signing path — `eip1193.rs` — shared with MetaMask, and Privy becomes "another
// provider" rather than a second parallel implementation of login, key
// derivation and message signing. Two implementations of a key derivation is how
// clients end up disagreeing about someone's identity.
//
// Built offline into `static/vendor/privy/privy.js` by `tools/build-privy.mjs`
// and checked in, because nothing in this app may be fetched from a CDN at
// runtime — see index.html.

import React, { useEffect } from "react";
import { createRoot } from "react-dom/client";
import {
  PrivyProvider,
  usePrivy,
  useWallets,
  useCreateWallet,
} from "@privy-io/react-auth";

// Filled in by <Bridge/> on every render, read by the promise API below. A
// mutable box rather than an event stream: the Rust side always wants the
// *current* state, never a history of it.
const live = {
  ready: false,
  authenticated: false,
  wallets: [],
  login: null,
  logout: null,
  createWallet: null,
};

function Bridge() {
  const { login, logout, authenticated, ready } = usePrivy();
  const { wallets } = useWallets();
  const { createWallet } = useCreateWallet();

  useEffect(() => {
    live.ready = ready;
    live.authenticated = authenticated;
    // Embedded wallets only. An external wallet reached *through* Privy would
    // be MetaMask by a longer road, and the Rust side already has that door.
    live.wallets = wallets.filter((w) => w.walletClientType === "privy");
    live.login = login;
    live.logout = logout;
    live.createWallet = createWallet;
  }, [ready, authenticated, wallets, login, logout, createWallet]);

  return null;
}

/// Resolve once `predicate()` holds, or reject after `timeoutMs`.
///
/// Privy's readiness and the arrival of an embedded wallet are both asynchronous
/// and neither is a promise the SDK hands out. Polling is the honest way to wait
/// on a React state transition from outside React.
function until(predicate, timeoutMs, what) {
  return new Promise((resolve, reject) => {
    if (predicate()) return resolve();
    const started = Date.now();
    const timer = setInterval(() => {
      if (predicate()) {
        clearInterval(timer);
        resolve();
      } else if (Date.now() - started > timeoutMs) {
        clearInterval(timer);
        // A timeout here is almost always "the person closed the modal", so the
        // message has to survive being shown to them.
        reject(new Error(what));
      }
    }, 120);
  });
}

let mounted = false;

const api = {
  /// Mount the provider. Idempotent — calling twice is a no-op, because the
  /// login screen can re-render and must not stack React roots.
  init(appId, chain) {
    if (mounted) return true;
    if (!appId) return false;

    const host = document.createElement("div");
    host.id = "ps-privy-root";
    // The Privy modal portals to <body> itself; this host only carries the
    // provider, so it must never occupy layout or take pointer events.
    host.style.display = "contents";
    document.body.appendChild(host);

    // `chain` comes from the server's /api/blockchain/info, so the wallet Privy
    // creates is on the same chain the rest of the app talks to.
    const supported = chain?.id
      ? [
          {
            id: chain.id,
            name: chain.name || `Chain ${chain.id}`,
            network: chain.name || String(chain.id),
            nativeCurrency: {
              name: chain.symbol || "CRO",
              symbol: chain.symbol || "CRO",
              decimals: 18,
            },
            rpcUrls: { default: { http: [chain.rpc] } },
            blockExplorers: chain.explorer
              ? { default: { name: "Explorer", url: chain.explorer } }
              : undefined,
          },
        ]
      : undefined;

    const config = {
      appearance: { theme: "dark", accentColor: "#22d3ee" },
      loginMethods: ["email"],
      embeddedWallets: { ethereum: { createOnLogin: "all-users" } },
      ...(supported ? { supportedChains: supported, defaultChain: supported[0] } : {}),
    };

    createRoot(host).render(
      React.createElement(
        PrivyProvider,
        { appId, config },
        React.createElement(Bridge),
      ),
    );
    mounted = true;
    return true;
  },

  /// Open Privy's modal and resolve once there is an embedded wallet.
  ///
  /// The reference client makes this two clicks — the first opens the modal and
  /// returns, the second proceeds. Here it is one call that waits, because from
  /// Rust the whole thing is a single `.await` and asking someone to press the
  /// same button twice is a worse interface, not a simpler one.
  async connect() {
    await until(() => live.ready, 20000, "Privy did not finish loading");

    if (!live.authenticated) {
      live.login();
      // Generous: this window contains a person reading an email and typing a
      // code out of it.
      await until(() => live.authenticated, 300000, "Sign-in was not completed");
    }

    // `createOnLogin: "all-users"` usually means a wallet is already there.
    await until(() => live.wallets.length > 0, 30000, "No wallet was created")
      .catch(async () => {
        if (!live.createWallet) throw new Error("No wallet was created");
        await live.createWallet();
        await until(() => live.wallets.length > 0, 30000, "No wallet was created");
      });

    return live.wallets[0].address;
  },

  /// The EIP-1193 provider for `address`, or the first wallet when omitted.
  /// This is the whole point of the bridge.
  async provider(address) {
    await until(() => live.wallets.length > 0, 30000, "No wallet available");
    const w =
      live.wallets.find(
        (x) => x.address.toLowerCase() === String(address || "").toLowerCase(),
      ) ?? live.wallets[0];
    return await w.getEthereumProvider();
  },

  addresses() {
    return live.wallets.map((w) => w.address);
  },

  authenticated() {
    return !!live.authenticated;
  },

  async disconnect() {
    if (live.logout) await live.logout();
  },
};

window.psPrivy = api;
