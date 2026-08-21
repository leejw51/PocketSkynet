/**
 * Boots a real `pocketskynet` process per test file, modeled on
 * `app/server/tests/common/harness.rs`.
 *
 * Every server gets its own ephemeral port and temp data directory. Ports are
 * picked by binding to 0 and releasing, which leaves a race window: another
 * process can win the bind, and our child's `/api/health` probe would then be
 * answered by the winner. So after health answers we check the child is still
 * alive ("is our child alive"), and retry the whole boot when it is not.
 *
 * Teardown kills the child and removes the data dir; a process-exit hook
 * backstops leaked children even when a test file crashes.
 */

import { execFileSync, spawn, ChildProcess } from "node:child_process";
import { randomBytes } from "node:crypto";
import { createSocket } from "node:dgram";
import { existsSync, mkdirSync, openSync, readFileSync, rmSync, statSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { isAbsolute, join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { Agent, fetch as undiciFetch } from "undici";
import { normalizePrivateKey } from "../../src/wallet.js";
import { appRoot } from "./vectors.js";

/** Handed to the server with `--jwt-secret` so tests can mint and tamper. */
export const JWT_SECRET = "pocketskynet-ts-integration-test-secret-0123456789abcdef";

const BOOT_TIMEOUT_MS = 30_000;
let counter = 0;

const liveChildren = new Set<ChildProcess>();
process.on("exit", () => {
  for (const child of liveChildren) {
    try {
      child.kill("SIGKILL");
    } catch {
      /* already gone */
    }
  }
});

/** Locate the pocketskynet binary; build it once with cargo when absent. */
export function resolveServerBinary(): string {
  const fromEnv = process.env["POCKETSKYNET_BIN"];
  if (fromEnv !== undefined && fromEnv.length > 0) {
    if (!existsSync(fromEnv)) {
      throw new Error(`POCKETSKYNET_BIN points at a missing file: ${fromEnv}`);
    }
    return fromEnv;
  }

  const roots = [appRoot()];
  // The checkout may be a git worktree of a normal repo (common dir is
  // `<main>/.git`) or of an absorbed submodule (common dir is
  // `<super>/.git/modules/<name>`, with the main checkout named by its
  // `core.worktree`). Either way, target/ lives with the main checkout.
  try {
    const commonDir = execFileSync(
      "git",
      ["rev-parse", "--path-format=absolute", "--git-common-dir"],
      { cwd: appRoot(), encoding: "utf8" },
    ).trim();
    const besideGitDir = join(commonDir, "..", "app");
    if (existsSync(besideGitDir)) roots.push(besideGitDir);
    try {
      const coreWorktree = execFileSync(
        "git",
        ["config", "--file", join(commonDir, "config"), "core.worktree"],
        { encoding: "utf8" },
      ).trim();
      if (coreWorktree.length > 0) {
        // core.worktree may be absolute or relative to the git dir. join()
        // silently concatenates an absolute second segment, so resolve the
        // absolute case explicitly rather than mis-joining it.
        const worktreeRoot = isAbsolute(coreWorktree)
          ? coreWorktree
          : join(commonDir, coreWorktree);
        const mainRepoApp = join(worktreeRoot, "app");
        if (existsSync(mainRepoApp)) roots.push(mainRepoApp);
      }
    } catch {
      /* no core.worktree — a normal repository */
    }
  } catch {
    /* not a git checkout — fine */
  }

  const candidates: string[] = [];
  for (const root of roots) {
    candidates.push(join(root, "target", "release", "pocketskynet"));
    candidates.push(join(root, "target", "debug", "pocketskynet"));
  }
  const existing = candidates.filter((p) => existsSync(p));
  if (existing.length > 0) {
    // Prefer the most recently built one.
    existing.sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs);
    return existing[0]!;
  }

  // Build once via cargo, then re-check.
  try {
    execFileSync("cargo", ["build", "-p", "pocketskynet"], {
      cwd: appRoot(),
      stdio: "inherit",
      timeout: 15 * 60 * 1000,
    });
  } catch (err) {
    throw new Error(
      `no pocketskynet binary found (looked at ${candidates.join(", ")}) and ` +
        `'cargo build -p pocketskynet' failed: ${err instanceof Error ? err.message : err}. ` +
        "Build the server or set POCKETSKYNET_BIN.",
    );
  }
  const built = join(appRoot(), "target", "debug", "pocketskynet");
  if (!existsSync(built)) {
    throw new Error(`cargo build succeeded but ${built} does not exist`);
  }
  return built;
}

function freeTcpPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.listen(0, "127.0.0.1", () => {
      const address = srv.address();
      if (address === null || typeof address === "string") {
        srv.close();
        reject(new Error("no TCP address"));
        return;
      }
      const port = address.port;
      srv.close(() => resolve(port));
    });
    srv.on("error", reject);
  });
}

/** TCP and UDP port numbers live in different namespaces — probe UDP itself. */
function freeUdpPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const sock = createSocket("udp4");
    sock.bind(0, "127.0.0.1", () => {
      const port = sock.address().port;
      sock.close(() => resolve(port));
    });
    sock.on("error", reject);
  });
}

function uniqueDir(): string {
  counter += 1;
  return join(tmpdir(), `ps-ts-it-${process.pid}-${Date.now()}-${counter}`);
}

/** Environment for the child: PS_ and VITE_ vars scrubbed, baked env ignored. */
function childEnv(extra: Record<string, string>): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  for (const key of Object.keys(env)) {
    if (key.startsWith("PS_") || key.startsWith("VITE_")) delete env[key];
  }
  delete env["POCKETSKYNET_PATH"];
  env["PS_IGNORE_BAKED_ENV"] = "1";
  for (const [key, value] of Object.entries(extra)) env[key] = value;
  return env;
}

export interface StartOptions {
  tls?: boolean;
  http3?: boolean;
  extraArgs?: string[];
  env?: Record<string, string>;
}

export class TestServer {
  private constructor(
    private readonly child: ChildProcess,
    readonly port: number,
    readonly http3Port: number | undefined,
    readonly dataDir: string,
    readonly baseUrl: string,
  ) {}

  get isTls(): boolean {
    return this.baseUrl.startsWith("https://");
  }

  url(path: string): string {
    return `${this.baseUrl}${path}`;
  }

  /** The CA this server generated (TLS and/or HTTP/3 servers). */
  caPath(): string {
    return join(this.dataDir, "tls", "ca.crt");
  }

  caPem(): string {
    return readFileSync(this.caPath(), "utf8");
  }

  http3Url(path: string): string {
    if (this.http3Port === undefined) throw new Error("server has no HTTP/3 listener");
    return `https://127.0.0.1:${this.http3Port}${path}`;
  }

  serverLog(): string {
    try {
      return readFileSync(join(this.dataDir, "server.log"), "utf8");
    } catch {
      return "";
    }
  }

  /** Kill the child and delete the data dir. Safe to call twice. */
  async stop(): Promise<void> {
    try {
      this.child.kill("SIGKILL");
    } catch {
      /* already gone */
    }
    liveChildren.delete(this.child);
    // Give the OS a beat to release the process before removing its cwd data.
    await sleep(30);
    try {
      rmSync(this.dataDir, { recursive: true, force: true });
    } catch {
      /* best effort */
    }
  }

  static async start(opts: StartOptions = {}): Promise<TestServer> {
    let lastErr = "";
    for (let attempt = 0; attempt < 5; attempt++) {
      try {
        return await TestServer.tryStart(opts);
      } catch (err) {
        lastErr = err instanceof Error ? err.message : String(err);
      }
    }
    throw new Error(`could not start pocketskynet after 5 attempts: ${lastErr}`);
  }

  private static async tryStart(opts: StartOptions): Promise<TestServer> {
    const binary = resolveServerBinary();
    const port = await freeTcpPort();
    const http3Port = opts.http3 ? await freeUdpPort() : undefined;
    const dataDir = uniqueDir();
    const staticDir = join(dataDir, "static");
    mkdirSync(staticDir, { recursive: true });

    const logPath = join(dataDir, "server.log");
    const logFd = openSync(logPath, "w");

    const args = [
      "--host",
      "127.0.0.1",
      "--port",
      String(port),
      "--data-dir",
      dataDir,
      "--static-dir",
      staticDir,
      "--jwt-secret",
      JWT_SECRET,
      "--no-rate-limit",
      "--no-payment-verify",
      "--no-mdns",
      "--log",
      "warn",
    ];
    if (opts.tls) args.push("--tls");
    if (opts.http3) args.push("--http3", "--http3-port", String(http3Port));
    if (opts.extraArgs) args.push(...opts.extraArgs);

    const child = spawn(binary, args, {
      env: childEnv(opts.env ?? {}),
      stdio: ["ignore", logFd, logFd],
    });
    liveChildren.add(child);

    const scheme = opts.tls ? "https" : "http";
    const baseUrl = `${scheme}://127.0.0.1:${port}`;
    const server = new TestServer(child, port, http3Port, dataDir, baseUrl);

    try {
      await server.awaitHealth();
      if (opts.http3 || opts.tls) await server.awaitCa();
      return server;
    } catch (err) {
      const log = server.serverLog().split("\n").slice(-30).join("\n");
      await server.stop();
      throw new Error(
        `${err instanceof Error ? err.message : err}\n--- server log (tail) ---\n${log}`,
      );
    }
  }

  private async awaitHealth(): Promise<void> {
    const deadline = Date.now() + BOOT_TIMEOUT_MS;
    // Over TLS, probe with verification off — the real tests then verify
    // against the generated CA explicitly.
    const dispatcher = this.isTls
      ? new Agent({ connect: { rejectUnauthorized: false } })
      : undefined;
    try {
      while (Date.now() < deadline) {
        if (this.child.exitCode !== null) {
          throw new Error(`server exited during boot with code ${this.child.exitCode}`);
        }
        try {
          const response = await undiciFetch(this.url("/api/health"), {
            ...(dispatcher !== undefined ? { dispatcher } : {}),
            signal: AbortSignal.timeout(2_000),
          });
          if (response.ok) {
            await response.body?.cancel();
            // Somebody answered — make sure it was our child. A child that
            // lost the bind race has already exited, and this 200 came from
            // the winner.
            if (this.child.exitCode !== null) {
              throw new Error(
                `another process owns port ${this.port}; our child exited with ${this.child.exitCode}`,
              );
            }
            return;
          }
          await response.body?.cancel();
        } catch (err) {
          if (err instanceof Error && /owns port|exited during boot/.test(err.message)) throw err;
          // Connection refused while booting — keep polling.
        }
        await sleep(25);
      }
      throw new Error(`/api/health never became ready on port ${this.port}`);
    } finally {
      await dispatcher?.close();
    }
  }

  /** `/api/health` answering does not prove the CA file is flushed yet. */
  private async awaitCa(): Promise<void> {
    const deadline = Date.now() + 10_000;
    while (!existsSync(this.caPath()) && Date.now() < deadline) {
      await sleep(20);
    }
    if (!existsSync(this.caPath())) {
      throw new Error(`server never wrote its CA at ${this.caPath()}`);
    }
  }
}

/** A fresh random private key (retrying the astronomically unlikely invalid). */
export function randomPrivateKeyHex(): string {
  for (;;) {
    const candidate = `0x${randomBytes(32).toString("hex")}`;
    try {
      normalizePrivateKey(candidate); // throws for 0 or >= n
      return candidate;
    } catch {
      /* try again */
    }
  }
}
