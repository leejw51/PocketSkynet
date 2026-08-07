# PocketSkynet — blockchain-authenticated messenger, Rust end to end.
#
#   make          Show this help
#   make start    Run in CLI mode — the server in this terminal, over HTTPS
#   make https    The same, said out loud
#   make http     Plain HTTP — localhost only; a LAN address cannot sign in
#   make stop     Stop a running server
#   make restart  Stop it, rebuild, and start it again
#   make refresh  Rebuild just the web UI — no server restart needed
#   make build    Release build: core, server, and the WASM client
#   make gui      Run the desktop app
#   make package  Bundle the desktop app as an installer
#   make test             Unit + integration
#   make unittest         Unit tests only
#   make integrationtest  Integration tests only (Rust suite + Python supervisor)
#   make format           Format all source code — Rust, Python, JavaScript/TypeScript

SHELL := /bin/bash
.DEFAULT_GOAL := help

CARGO       ?= cargo
TRUNK       ?= trunk
# Deployment settings, sourced by `start` with `set -a` so every assignment is
# exported to the server. Applied *over* the surrounding shell deliberately: a
# profile that exports VITE_CHAIN_ID for some other project must not decide
# which chain this deployment runs on. Absent is fine — the defaults compiled
# into the server are the same ones .env.example ships with.
ENV_FILE    ?= .env
PORT        ?= 9099
HOST        ?= 0.0.0.0
# The persistence root, resolved the same way the binary resolves it:
# `.env`'s POCKETSKYNET_PATH, else the surrounding shell's, else
# ~/.pocketskynet. `make start DATA_DIR=…` still overrides everything.
DATA_DIR    ?= $(shell [ -f "$(ENV_FILE)" ] && . "./$(ENV_FILE)"; echo $${POCKETSKYNET_PATH:-$$HOME/.pocketskynet})
PROFILE     ?= release
# HTTPS is the default, because the reduced platform a browser hands a
# plain-HTTP page on a LAN address is not a smaller version of this app — it is
# a broken one. `crypto.subtle` is absent outside a secure context, and without
# it there is no wallet sign-in at all: the page loads and then cannot log in.
# Every phone and tablet reaches this server on a LAN or VPN address, so the
# useful default is the one they can actually use. The certificate is
# self-signed into $(DATA_DIR)/tls on first run and reused after.
#
# `make start TLS=0` (or `make http`) opts out, for a localhost-only session
# where the secure-context exemption applies and a cert warning is just noise.
TLS         ?= 1
# Normalised once: `TLS=0` must mean off everywhere, including `dev.sh`, which
# keys on its argument being non-empty rather than on its value.
TLS_ON      := $(filter-out 0,$(TLS))
TLS_FLAG    := $(if $(TLS_ON),--tls,)

# HTTP/3 over QUIC, on a UDP port beside the TCP one. On by default, like TLS:
# both listeners serve the same API, a client picks whichever it can, and the
# phone clients this project exists for are exactly the case QUIC helps — no
# transport head-of-line blocking, one round trip to first byte, and a
# connection that survives Wi-Fi→cellular.
#
# It shares the HTTPS port number: TCP and UDP are separate namespaces, and
# pointing `Alt-Svc` at the number the client already knows is the conventional
# deployment. `HTTP3_PORT=…` moves it; `make start HTTP3=0` turns it off.
#
# Worth knowing: a client can only *use* HTTP/3 over https, because QUIC has no
# plaintext mode. `make http` (TLS=0) therefore brings the QUIC listener up on a
# port nothing can reach — see the README.
HTTP3       ?= 1
HTTP3_ON    := $(filter-out 0,$(HTTP3))
HTTP3_PORT  ?= $(PORT)
HTTP3_FLAG  := $(if $(HTTP3_ON),--http3 --http3-port $(HTTP3_PORT),)

# The deployment settings the server compiles in (see server/build.rs). The
# desktop app is launched by Finder with no shell to source `.env`, so these
# have to be present at *build* time or an installed app has no payment wallet
# and every paid feature fails. Sourced here for the same reason `start` sources
# it: `.env` decides, not whatever the surrounding shell happens to export —
# otherwise a developer's exported VITE_CHAIN_ID silently ends up baked into a
# release. In CI there is no `.env` and the values come from the environment
# the workflow sets from repository secrets.
BAKE_ENV := set -a; if [ -f "$(ENV_FILE)" ]; then . "./$(ENV_FILE)"; fi; set +a;

VERSION     := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
TARGET      := $(shell rustc -vV | sed -n 's/^host: //p')
BIN         := target/$(PROFILE)/pocketskynet
WEB_DIST    := web/dist

ifeq ($(PROFILE),release)
CARGO_PROFILE_FLAG := --release
TRUNK_PROFILE_FLAG := --release
else
CARGO_PROFILE_FLAG :=
TRUNK_PROFILE_FLAG :=
endif

.PHONY: help start https http http3 stop restart refresh build gui package test \
        web server tools dev \
        unittest integrationtest test-integration-rust test-integration-python test-wasm \
        package-installer verify fmt format fmt-check fmt-tools lint clean distclean assets assets-force assets-cute assets-human db-reset

## ----------------------------------------------------------------- help ----

help: ## Show this help
	@echo ""
	@echo "  PocketSkynet $(VERSION) — $(TARGET)"
	@echo ""
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| sort \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "  Variables: PORT=$(PORT) HOST=$(HOST) DATA_DIR=$(DATA_DIR) PROFILE=$(PROFILE)"
	@echo ""

## ---------------------------------------------------------------- build ----

build: web server ## Build everything in release mode

web: tools ## Compile the Yew client to WebAssembly into web/dist
	@echo "==> web client (wasm32, $(PROFILE))"
	cd web && $(TRUNK) build $(TRUNK_PROFILE_FLAG)

# `web` under the name people reach for after changing the UI. It is a separate
# target because the instinct after a client-side edit is `make restart`, which
# stops the server, rebuilds the Rust binary too, and starts it again — none of
# which a changed component needs. The server serves the client straight out of
# `web/dist` as a static directory, so replacing the bundle is the whole job and
# an already-running server hands out the new one on the next request.
#
# Reloading the tab is enough to see it, and the service worker does not get in
# the way: `web/pwa/sw.js` serves the document network-first, and every asset it
# does answer from cache is content-hashed, so a new build is a new URL.
refresh: web ## Rebuild the web UI — reload the browser to pick it up
	@echo ""
	@echo "  ✓ $(WEB_DIST) rebuilt — reload the page"
	@echo ""

server: ## Compile the server binary
	@echo "==> server ($(PROFILE))"
	@$(BAKE_ENV) $(CARGO) build $(CARGO_PROFILE_FLAG) -p pocketskynet-server

tools: ## Check the wasm toolchain is present
	@rustup target list --installed | grep -qx wasm32-unknown-unknown \
		|| { echo "installing wasm32-unknown-unknown..."; rustup target add wasm32-unknown-unknown; }
	@command -v $(TRUNK) >/dev/null \
		|| { echo "ERROR: trunk not found. Install with: cargo install trunk"; exit 1; }

## ---------------------------------------------------------------- start ----

# CLI mode: the server runs in this terminal and prints the URLs to share.
# Ctrl-C stops it; `make stop` does the same from another terminal.
start: build ## Run in CLI mode — the server in this terminal (HTTPS)
	@mkdir -p $(DATA_DIR)
	@set -a; if [ -f "$(ENV_FILE)" ]; then . "./$(ENV_FILE)"; fi; set +a; \
	 exec $(BIN) --host $(HOST) --port $(PORT) --data-dir $(DATA_DIR) --static-dir $(WEB_DIST) $(TLS_FLAG) $(HTTP3_FLAG)

# Kept as its own target because it is the name people reach for and look up,
# and because it should still say HTTPS out loud even now that it is what
# `start` already does.
https: ## Run over HTTPS with a self-signed certificate (same as start)
	@$(MAKE) --no-print-directory start TLS=1

# The opt-out. Only sound when nothing but this machine connects: 127.0.0.1 is
# a secure context by exemption, so sign-in works there over plain HTTP and a
# self-signed cert would only cost a browser warning.
http: ## Run over plain HTTP — localhost only, no wallet sign-in from a LAN address
	@$(MAKE) --no-print-directory start TLS=0

# Kept as its own target for the same reason `https` is: it is the name people
# reach for, and it should still say HTTP/3 out loud now that it is what
# `start` already does.
http3: ## Run HTTPS + HTTP/3 together (same as start)
	@$(MAKE) --no-print-directory start TLS=1 HTTP3=1

# One process owns both the HTTPS port and the redirect port beside it, so
# stopping it on the one the operator knows about releases both.
stop: ## Stop a running server
	@./scripts/stop.sh $(PORT)

# A sequence, not a race: stop.sh waits for the process to exit before it
# returns, so the port is free by the time `start` binds it. It also exits
# non-zero when the port is held by something that is not PocketSkynet, and
# that stops the restart here rather than starting a second server beside it.
# Nothing running is not an error — this doubles as "start it, whatever the
# current state". `start` rebuilds first, so a restart picks up your changes.
# TLS carries through, so `make restart TLS=0` comes back up over plain HTTP.
restart: ## Stop a running server, rebuild, and start it again
	@$(MAKE) --no-print-directory stop
	@$(MAKE) --no-print-directory start

dev: tools ## Hot-reloading development mode
	@mkdir -p $(DATA_DIR)
	@set -a; if [ -f "$(ENV_FILE)" ]; then . "./$(ENV_FILE)"; fi; set +a; \
	 ./scripts/dev.sh $(HOST) $(PORT) $(DATA_DIR) $(TLS_ON)

## ------------------------------------------------------------------ gui ----

# The desktop app embeds the server, so it needs the WASM bundle on disk but no
# running server — `web`, not `build`.
gui: web ## Run the desktop app
	@echo "==> desktop app"
	$(CARGO) run $(CARGO_PROFILE_FLAG) -p pocketskynet-gui

# Run from `gui/`: the Tauri CLI locates the app by finding `tauri.conf.json`
# next to the crate it is building.
#
UNAME_S := $(shell uname -s)
APP_BUNDLE := target/release/bundle/macos/PocketSkynet.app
DMG_OUT    := target/release/bundle/dmg/PocketSkynet_$(VERSION)_$(shell uname -m).dmg
GUI_BIN    := pocketskynet-gui

# The app bundle and the installer are separate `tauri` invocations because
# Tauri's own DMG step drives Finder over AppleScript and dies without a desktop
# session. `scripts/make-dmg.sh` replaces it with plain `hdiutil`, so packaging
# works over ssh and in CI.
package: web ## Bundle the desktop app
	@$(CARGO) tauri --version >/dev/null 2>&1 \
		|| { echo "ERROR: tauri CLI not found. Install with: cargo install tauri-cli --version '^2'"; exit 1; }
	@echo "==> bundling PocketSkynet $(VERSION)"
	@# Ad-hoc signing unless the caller provides a real identity (CI exports
	@# APPLE_SIGNING_IDENTITY plus the notarization credentials). `BAKE_ENV`
	@# supplies the deployment settings the server compiles in — this bundle is
	@# the one build that cannot fall back to reading them at runtime.
	@$(BAKE_ENV) cd gui && APPLE_SIGNING_IDENTITY="$${APPLE_SIGNING_IDENTITY:--}" $(CARGO) tauri build --bundles app
	@$(MAKE) --no-print-directory package-installer
	@echo ""
	@echo "  built:"
	@find target/release/bundle -maxdepth 3 \
		\( -name '*.dmg' -o -name '*.app' -o -name '*.deb' -o -name '*.AppImage' -o -name '*.msi' -o -name '*.exe' \) \
		-not -name 'rw.*' \
		-print 2>/dev/null | sed 's/^/    /' || true
	@echo ""
	@echo "  compiled binary:"
	@test -f "$(CURDIR)/target/release/$(GUI_BIN)" \
		&& echo "    $(CURDIR)/target/release/$(GUI_BIN)" || true
	@test -f "$(CURDIR)/$(APP_BUNDLE)/Contents/MacOS/$(GUI_BIN)" \
		&& echo "    $(CURDIR)/$(APP_BUNDLE)/Contents/MacOS/$(GUI_BIN)" || true

package-installer: ## Wrap the bundled app as an installer (.dmg on macOS)
ifeq ($(UNAME_S),Darwin)
	@# Scratch images left by an interrupted `hdiutil` run would otherwise be
	@# picked up as build output.
	@find target/release/bundle -name 'rw.*.dmg' -delete 2>/dev/null || true
	@test -d "$(APP_BUNDLE)" \
		|| { echo "ERROR: $(APP_BUNDLE) not found — run 'make package' first"; exit 1; }
	@./scripts/make-dmg.sh "$(APP_BUNDLE)" "$(DMG_OUT)" "PocketSkynet"
else
	@echo "==> installer"
	cd gui && $(CARGO) tauri build --bundles deb appimage
endif

## ----------------------------------------------------------------- test ----

test: unittest integrationtest ## Unit and integration tests

# `web` is its own workspace, so `--workspace` does not reach it. Its logic
# tests are pure Rust and run on the host; none needs a browser — which is
# what makes them unit tests despite living in the client.
unittest: ## Unit tests — workspace libraries and binaries, plus web client logic
	@echo "==> unit tests"
	$(CARGO) test --workspace --lib --bins
	@echo "==> web client tests"
	cd web && $(CARGO) test

# Two halves, both hermetic. The Rust suite boots one server per test file
# and probes edges (TLS, HTTP/3, compression, tampered JWTs). The Python
# supervisor boots one shared backend on its own port with its own data
# directory, walks every user-facing flow end to end — auth, rooms, DMs,
# messages, threads, reactions, mentions, presence, blocking, sync, search,
# files, uploads, E2EE keys, SSE, administration (blockchain excluded) —
# then stops the backend and deletes the directory, pass or fail. A second
# backend boots with --tls --http3 to prove the self-signed-certificate
# deployment: a CA minted on the fly that verifies, HTTP/3 advertised, the
# QUIC listener answering, and the HTTP→HTTPS redirect in place.
integrationtest: test-integration-rust test-integration-python ## Integration tests — Rust suite + Python supervisor

test-integration-rust: ## Rust end-to-end tests against servers they start themselves
	@echo "==> integration tests (rust)"
	$(CARGO) test --workspace --test '*'

test-integration-python: ## Full-flow suite via the Python supervisor (tests/integration)
	@echo "==> integration tests (python supervisor)"
	$(CARGO) build -p pocketskynet-server
	python3 tests/integration/run.py

test-browser: ## Large-file transfer checks against a *running* server (see tests/browser/README.md)
	@echo "==> browser transfer tests (run 'make restart' first)"
	cd tests/browser && node upload.js && node resume.js

test-wasm: tools ## Type-check the client for wasm32
	$(CARGO) check --manifest-path web/Cargo.toml --target wasm32-unknown-unknown

## --------------------------------------------------------------- verify ----

verify: fmt-check lint test test-wasm ## Everything CI would run
	@echo ""
	@echo "  ✓ verify passed"

# Formatting covers every language in the tree: Rust via rustfmt, Python via
# black, JavaScript/TypeScript via prettier. The file lists come from git, so
# node_modules and build output can never be rewritten; the vendored Privy
# bundle is the one tracked file excluded — it is built output, not source.
PY_SRC   := $(shell git ls-files '*.py')
JS_SRC   := $(shell git ls-files '*.js' '*.mjs' '*.ts' '*.tsx' | grep -v '^web/static/vendor/')
BLACK    ?= black
# Pinned so `make fmt` here and `make fmt-check` in CI can never disagree
# about what formatted means.
PRETTIER ?= npx --yes prettier@3.6.2

fmt-tools: ## Check the non-Rust formatters are present
	@command -v $(firstword $(BLACK)) >/dev/null \
		|| { echo "ERROR: black not found. Install with: pip3 install black"; exit 1; }
	@command -v npx >/dev/null \
		|| { echo "ERROR: npx not found. Install Node.js"; exit 1; }

fmt: fmt-tools ## Format all source code — Rust, Python, JavaScript/TypeScript
	$(CARGO) fmt --all
	cd web && $(CARGO) fmt --all
	$(BLACK) --quiet $(PY_SRC)
	$(PRETTIER) --write --log-level warn $(JS_SRC)

format: fmt ## Format all source code (alias for fmt)

fmt-check: fmt-tools ## Fail if any source is unformatted
	$(CARGO) fmt --all -- --check
	cd web && $(CARGO) fmt --all -- --check
	$(BLACK) --check --quiet $(PY_SRC)
	$(PRETTIER) --check --log-level warn $(JS_SRC)

lint: tools ## Clippy, warnings denied
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	cd web && $(CARGO) clippy --target wasm32-unknown-unknown --all-targets -- -D warnings

## --------------------------------------------------------------- assets ----

privy: ## Rebuild the vendored Privy bundle (needs ../server/node_modules)
	node tools/build-privy.mjs

assets: ## Generate missing imagery for every skin (needs GROK_API_KEY)
	python3 tools/genart.py

assets-force: ## Regenerate all imagery, every skin
	python3 tools/genart.py --force

assets-cute: ## Generate missing imagery for the cuteskynet skin only
	python3 tools/genart.py --skin cuteskynet

assets-human: ## Generate missing imagery for the humanskynet skin only
	python3 tools/genart.py --skin humanskynet

## --------------------------------------------------------------- chores ----

db-reset: ## Delete the database and event log
	@rm -rf $(DATA_DIR)
	@echo "    $(DATA_DIR)/ removed"

clean: ## Remove build output
	$(CARGO) clean
	cd web && $(CARGO) clean
	rm -rf $(WEB_DIST) dist

distclean: clean db-reset ## Remove build output and runtime data
