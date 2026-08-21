/**
 * Transports. One interface, two implementations:
 *
 * - **HTTP/1.1(+TLS)** via undici `fetch`. `--insecure` (self-signed dev
 *   certs) needs an undici `Agent` with `connect.rejectUnauthorized: false`
 *   because plain fetch has no per-request TLS knob. A CA can be pinned
 *   instead, which trusts that CA and only that CA.
 *
 * - **HTTP/3** by spawning an HTTP/3-capable `curl` via `execFile` (argv
 *   array, never a shell). Node 24 has no built-in HTTP/3 client (`node:quic`
 *   is not exposed even behind `--experimental-quic`), and npm has no
 *   maintained pure-JS HTTP/3 client, so curl is the pragmatic path. When no
 *   HTTP/3-capable curl exists on the machine the transport fails fast with a
 *   clear error instead of pretending.
 */

import { execFile } from "node:child_process";
import { readFileSync } from "node:fs";
import { promisify } from "node:util";
import { Agent, fetch as undiciFetch } from "undici";
import { TransportError } from "./errors.js";

const execFileAsync = promisify(execFile);

export type TransportKind = "http1" | "http3-curl";

export interface TransportRequest {
  method: "GET" | "POST" | "PUT" | "PATCH" | "DELETE";
  /** Path starting with `/`, e.g. `/api/health`. */
  path: string;
  /** JSON body; `undefined` sends no body. Serialized with JSON.stringify. */
  body?: unknown;
  /** JWT for the `Authorization: Bearer` header. */
  token?: string;
}

export interface TransportResponse {
  status: number;
  headers: Record<string, string>;
  bodyText: string;
}

export interface Transport {
  readonly kind: TransportKind;
  readonly baseUrl: string;
  request(req: TransportRequest): Promise<TransportResponse>;
  close(): Promise<void>;
}

export interface TransportOptions {
  /** e.g. `http://127.0.0.1:9099` or `https://host:9101`. */
  baseUrl: string;
  /** Use the HTTP/3 (curl-based) transport. */
  http3?: boolean;
  /** Accept self-signed certificates. */
  insecure?: boolean;
  /** Trust exactly this CA (PEM). HTTP/1.1 transport only. */
  caPem?: string;
  /** Trust exactly this CA (path to PEM file). */
  caPath?: string;
  /** Explicit curl binary for HTTP/3 (else probed). */
  curlPath?: string;
  timeoutMs?: number;
}

const DEFAULT_TIMEOUT_MS = 15_000;

function normalizeBaseUrl(baseUrl: string): string {
  let url: URL;
  try {
    url = new URL(baseUrl);
  } catch {
    throw new TransportError(`invalid server URL: ${JSON.stringify(baseUrl)}`);
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new TransportError(`server URL must be http(s), got ${url.protocol}//`);
  }
  return baseUrl.replace(/\/+$/, "");
}

function validatePath(path: string): void {
  if (!path.startsWith("/") || /[\s\x00-\x1f\x7f]/.test(path)) {
    throw new TransportError(`invalid request path: ${JSON.stringify(path)}`);
  }
}

/**
 * A bearer token that is safe to place in a header. JWTs are base64url
 * segments; anything with whitespace or control characters is refused so a
 * hostile "token" can never smuggle a second header or a second curl flag.
 */
export function validateToken(token: string): void {
  if (token.length === 0 || !/^[\x21-\x7e]+$/.test(token)) {
    throw new TransportError("token contains characters not allowed in an HTTP header");
  }
}

/* ------------------------------------------------------------------ */
/* HTTP/1.1 via undici fetch                                          */
/* ------------------------------------------------------------------ */

class FetchTransport implements Transport {
  readonly kind: TransportKind = "http1";
  readonly baseUrl: string;
  private readonly timeoutMs: number;
  private readonly dispatcher: Agent | undefined;

  constructor(opts: TransportOptions) {
    this.baseUrl = normalizeBaseUrl(opts.baseUrl);
    this.timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    const isHttps = this.baseUrl.startsWith("https://");
    if (isHttps && (opts.insecure || opts.caPem || opts.caPath)) {
      const connect: { rejectUnauthorized?: boolean; ca?: string } = {};
      if (opts.insecure) connect.rejectUnauthorized = false;
      const ca = opts.caPem ?? (opts.caPath ? readFileUtf8(opts.caPath) : undefined);
      if (ca !== undefined) connect.ca = ca;
      this.dispatcher = new Agent({ connect });
    }
  }

  async request(req: TransportRequest): Promise<TransportResponse> {
    validatePath(req.path);
    if (req.token !== undefined) validateToken(req.token);
    const headers: Record<string, string> = { accept: "application/json" };
    if (req.token !== undefined) headers["authorization"] = `Bearer ${req.token}`;
    let body: string | undefined;
    if (req.body !== undefined) {
      headers["content-type"] = "application/json";
      body = JSON.stringify(req.body);
    }
    let response;
    try {
      response = await undiciFetch(`${this.baseUrl}${req.path}`, {
        method: req.method,
        headers,
        ...(body !== undefined ? { body } : {}),
        ...(this.dispatcher !== undefined ? { dispatcher: this.dispatcher } : {}),
        signal: AbortSignal.timeout(this.timeoutMs),
      });
    } catch (err) {
      throw new TransportError(
        `request to ${this.baseUrl}${req.path} failed: ${describeFetchError(err)}`,
        err,
      );
    }
    const bodyText = await response.text();
    const responseHeaders: Record<string, string> = {};
    response.headers.forEach((value, key) => {
      responseHeaders[key.toLowerCase()] = value;
    });
    return { status: response.status, headers: responseHeaders, bodyText };
  }

  async close(): Promise<void> {
    await this.dispatcher?.close();
  }
}

function readFileUtf8(path: string): string {
  return readFileSync(path, "utf8");
}

function describeFetchError(err: unknown): string {
  if (err instanceof Error) {
    const causeMessage =
      err.cause instanceof Error ? err.cause.message : err.cause ? String(err.cause) : "";
    return causeMessage.length > 0 ? `${err.message} (${causeMessage})` : err.message;
  }
  return String(err);
}

/* ------------------------------------------------------------------ */
/* HTTP/3 via curl                                                    */
/* ------------------------------------------------------------------ */

/** Default places an HTTP/3-capable curl might live. */
export const DEFAULT_CURL_CANDIDATES: readonly string[] = [
  "/opt/homebrew/opt/curl/bin/curl",
  "/usr/local/opt/curl/bin/curl",
  "curl",
];

/**
 * Probe candidate curl binaries and return the first whose
 * `curl --version` feature list advertises HTTP3, or `null` when none does.
 */
export async function findHttp3Curl(
  candidates: readonly string[] = defaultCurlCandidates(),
): Promise<string | null> {
  for (const candidate of candidates) {
    try {
      const { stdout } = await execFileAsync(candidate, ["--version"], { timeout: 5_000 });
      if (/^Features:.*\bHTTP3\b/m.test(stdout)) return candidate;
    } catch {
      // Missing binary or a broken one — try the next candidate.
    }
  }
  return null;
}

function defaultCurlCandidates(): readonly string[] {
  const fromEnv = process.env["PSKYNET_CURL"];
  return fromEnv ? [fromEnv, ...DEFAULT_CURL_CANDIDATES] : DEFAULT_CURL_CANDIDATES;
}

const STATUS_MARKER = "\n__pskynet_http_status__:";

export interface CurlArgOptions {
  insecure?: boolean;
  caPath?: string;
  timeoutMs?: number;
}

/**
 * Build the curl argv (no shell is ever involved — `execFile` passes this
 * array straight to the kernel, so quoting is a non-issue; validation exists
 * to keep hostile values from being *interpreted by curl* as extra flags or
 * extra headers).
 */
export function buildCurlArgs(
  baseUrl: string,
  req: TransportRequest,
  opts: CurlArgOptions = {},
): string[] {
  const base = normalizeBaseUrl(baseUrl);
  if (!base.startsWith("https://")) {
    throw new TransportError("HTTP/3 requires an https:// server URL (QUIC mandates TLS)");
  }
  validatePath(req.path);
  const timeoutSec = Math.max(1, Math.ceil((opts.timeoutMs ?? DEFAULT_TIMEOUT_MS) / 1000));
  const args: string[] = [
    "--http3-only",
    "--silent",
    "--show-error",
    "--max-time",
    String(timeoutSec),
    "--request",
    req.method,
    "--header",
    "accept: application/json",
    "--write-out",
    `${STATUS_MARKER}%{response_code}`,
  ];
  if (req.token !== undefined) {
    validateToken(req.token);
    args.push("--header", `authorization: Bearer ${req.token}`);
  }
  if (req.body !== undefined) {
    // The body is a single argv element; curl never re-parses it as flags
    // because it follows `--data-binary`, and no shell ever sees it.
    args.push("--header", "content-type: application/json");
    args.push("--data-binary", JSON.stringify(req.body));
  }
  if (opts.insecure) args.push("--insecure");
  else if (opts.caPath !== undefined) args.push("--cacert", opts.caPath);
  args.push("--", `${base}${req.path}`);
  return args;
}

/** Parse curl stdout produced with the {@link STATUS_MARKER} write-out. */
export function parseCurlOutput(stdout: string): { status: number; bodyText: string } {
  const at = stdout.lastIndexOf(STATUS_MARKER);
  if (at < 0) throw new TransportError("curl produced no status marker (transport failure)");
  const bodyText = stdout.slice(0, at);
  const status = Number(stdout.slice(at + STATUS_MARKER.length).trim());
  if (!Number.isInteger(status) || status < 100 || status > 599) {
    throw new TransportError(`curl reported an unparseable HTTP status: ${JSON.stringify(stdout.slice(at))}`);
  }
  return { status, bodyText };
}

export class CurlHttp3Transport implements Transport {
  readonly kind: TransportKind = "http3-curl";
  readonly baseUrl: string;
  private readonly opts: TransportOptions;
  private curlPath: string | null | undefined;

  constructor(opts: TransportOptions) {
    this.baseUrl = normalizeBaseUrl(opts.baseUrl);
    this.opts = opts;
    this.curlPath = opts.curlPath;
  }

  /** Resolve (and cache) the curl binary; throws the fail-fast error. */
  private async resolveCurl(): Promise<string> {
    if (this.curlPath === undefined) {
      this.curlPath = await findHttp3Curl();
    }
    if (this.curlPath === null) {
      throw new TransportError(
        "HTTP/3 requested, but no HTTP/3-capable curl was found. " +
          "Node has no built-in HTTP/3 client, so this transport shells out to curl. " +
          "Install one (e.g. `brew install curl`, which ships HTTP3 on current Homebrew) " +
          "or point PSKYNET_CURL at an HTTP/3-enabled curl binary.",
      );
    }
    return this.curlPath;
  }

  async request(req: TransportRequest): Promise<TransportResponse> {
    const curl = await this.resolveCurl();
    const argOpts: CurlArgOptions = {};
    if (this.opts.insecure !== undefined) argOpts.insecure = this.opts.insecure;
    if (this.opts.caPath !== undefined) argOpts.caPath = this.opts.caPath;
    if (this.opts.timeoutMs !== undefined) argOpts.timeoutMs = this.opts.timeoutMs;
    const args = buildCurlArgs(this.baseUrl, req, argOpts);
    let stdout: string;
    try {
      const result = await execFileAsync(curl, args, {
        timeout: (this.opts.timeoutMs ?? DEFAULT_TIMEOUT_MS) + 5_000,
        maxBuffer: 32 * 1024 * 1024,
      });
      stdout = result.stdout;
    } catch (err) {
      const stderr =
        typeof err === "object" && err !== null && "stderr" in err
          ? String((err as { stderr: unknown }).stderr).trim()
          : "";
      throw new TransportError(
        `curl (HTTP/3) request to ${this.baseUrl}${req.path} failed${stderr ? `: ${stderr}` : ""}`,
        err,
      );
    }
    const { status, bodyText } = parseCurlOutput(stdout);
    return { status, headers: {}, bodyText };
  }

  async close(): Promise<void> {
    // Nothing persistent to tear down; each request is one curl process.
  }
}

/* ------------------------------------------------------------------ */

export function createTransport(opts: TransportOptions): Transport {
  return opts.http3 ? new CurlHttp3Transport(opts) : new FetchTransport(opts);
}
