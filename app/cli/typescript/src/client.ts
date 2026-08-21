/**
 * High-level PocketSkynet client: challenge → EIP-191 sign → JWT, then
 * rooms/messages over whichever transport was selected.
 */

import { personalSign } from "./eip191.js";
import { ApiError, apiErrorFromBody } from "./errors.js";
import { generatedUsername, msgHashPlaintext } from "./protocol.js";
import {
  createTransport,
  Transport,
  TransportOptions,
  TransportRequest,
} from "./transport.js";
import {
  ChallengeResponse,
  HealthResponse,
  LoginRequestBody,
  LoginResponse,
  MessageWithSender,
  Room,
  RoomWithMembers,
  SendMessageBody,
} from "./types.js";
import { accountFromPrivateKey } from "./wallet.js";

export interface ClientOptions extends TransportOptions {
  /** Wallet private key (hex, `0x` optional). Needed for `login`. */
  privateKey?: string;
  /** Username for first-time logins. Generated when omitted and needed. */
  username?: string;
  /** Pre-existing JWT; skips login for authenticated calls. */
  token?: string;
}

export interface LoginResult {
  response: LoginResponse;
  walletAddress: string;
  /** The username the login actually used, when one was sent. */
  usernameSent: string | undefined;
  /** True when the first attempt hit first-time-login and we retried. */
  retriedWithGeneratedUsername: boolean;
}

/**
 * Build the login request body. `undefined` username is **omitted** (never
 * `null` — the server treats any non-string as "no username", but sending
 * the key at all is a different wire byte sequence and this client mirrors
 * JS `JSON.stringify` omission semantics exactly).
 */
export function buildLoginBody(params: {
  walletAddress: string;
  challengeId: string;
  signature: string;
  username?: string | undefined;
}): LoginRequestBody {
  const body: LoginRequestBody = {
    walletAddress: params.walletAddress,
    challengeId: params.challengeId,
    signature: params.signature,
  };
  if (params.username !== undefined && params.username.length > 0) {
    body.username = params.username;
  }
  return body;
}

export class PocketSkynetClient {
  readonly transport: Transport;
  private readonly opts: ClientOptions;
  private jwt: string | undefined;

  constructor(opts: ClientOptions) {
    this.opts = opts;
    this.transport = createTransport(opts);
    this.jwt = opts.token;
  }

  get token(): string | undefined {
    return this.jwt;
  }

  set token(value: string | undefined) {
    this.jwt = value;
  }

  get walletAddress(): string {
    if (this.opts.privateKey === undefined) {
      throw new Error("no private key configured (pass privateKey or POCKETSKYNET_KEY)");
    }
    return accountFromPrivateKey(this.opts.privateKey).address;
  }

  async close(): Promise<void> {
    await this.transport.close();
  }

  /* ---------------- raw request plumbing ---------------- */

  private async requestJson<T>(
    method: TransportRequest["method"],
    path: string,
    body?: unknown,
    opts: { auth?: boolean } = {},
  ): Promise<T> {
    const req: TransportRequest = { method, path };
    if (body !== undefined) req.body = body;
    if (opts.auth) {
      const token = await this.ensureToken();
      req.token = token;
    }
    const response = await this.transport.request(req);
    if (response.status < 200 || response.status >= 300) {
      throw apiErrorFromBody(response.status, response.bodyText);
    }
    return parseJsonBody<T>(response.bodyText);
  }

  /** The cached JWT, logging in first when a key is available. */
  async ensureToken(): Promise<string> {
    if (this.jwt !== undefined) return this.jwt;
    if (this.opts.privateKey === undefined) {
      throw new Error("not logged in and no private key configured");
    }
    await this.login();
    return this.jwt!;
  }

  /* ---------------- endpoints ---------------- */

  async health(): Promise<HealthResponse> {
    return this.requestJson<HealthResponse>("GET", "/api/health");
  }

  async requestChallenge(walletAddress: string): Promise<ChallengeResponse> {
    return this.requestJson<ChallengeResponse>("POST", "/api/auth/challenge", {
      walletAddress,
    });
  }

  /**
   * Full login flow. Signs the challenge message **verbatim**. A failed
   * login burns its challenge, so the first-time-login retry requests a
   * fresh challenge before sending the generated username.
   */
  async login(): Promise<LoginResult> {
    if (this.opts.privateKey === undefined) {
      throw new Error("login requires a private key");
    }
    const account = accountFromPrivateKey(this.opts.privateKey);
    const address = account.address;

    const attempt = async (username: string | undefined): Promise<LoginResponse> => {
      const challenge = await this.requestChallenge(address);
      const signature = personalSign(challenge.message, account.privateKey);
      const body = buildLoginBody({
        walletAddress: address,
        challengeId: challenge.challengeId,
        signature,
        username,
      });
      return this.requestJson<LoginResponse>("POST", "/api/auth/login", body);
    };

    let usernameSent = this.opts.username;
    let retried = false;
    let response: LoginResponse;
    try {
      response = await attempt(usernameSent);
    } catch (err) {
      const firstTime =
        err instanceof ApiError &&
        err.status === 400 &&
        err.message === "Username is required for first-time login";
      if (!firstTime || usernameSent !== undefined) throw err;
      usernameSent = generatedUsername(address);
      retried = true;
      response = await attempt(usernameSent);
    }
    this.jwt = response.token;
    return {
      response,
      walletAddress: address,
      usernameSent,
      retriedWithGeneratedUsername: retried,
    };
  }

  async rooms(): Promise<RoomWithMembers[]> {
    return this.requestJson<RoomWithMembers[]>("GET", "/api/rooms", undefined, { auth: true });
  }

  async room(roomId: string): Promise<RoomWithMembers> {
    return this.requestJson<RoomWithMembers>(
      "GET",
      `/api/rooms/${encodeURIComponent(roomId)}`,
      undefined,
      { auth: true },
    );
  }

  async createRoom(name: string, description?: string): Promise<Room> {
    const body: { name: string; description?: string } = { name };
    if (description !== undefined) body.description = description;
    return this.requestJson<Room>("POST", "/api/rooms", body, { auth: true });
  }

  /**
   * Send a plaintext message. `msgHash` = SHA-256 of the **trimmed** content
   * (the server trims before storing).
   */
  async sendMessage(roomId: string, content: string): Promise<MessageWithSender> {
    const body: SendMessageBody = {
      content,
      msgHash: msgHashPlaintext(content),
    };
    return this.requestJson<MessageWithSender>(
      "POST",
      `/api/rooms/${encodeURIComponent(roomId)}/messages`,
      body,
      { auth: true },
    );
  }

  async messages(
    roomId: string,
    opts: { limit?: number; since?: number; before?: number } = {},
  ): Promise<MessageWithSender[]> {
    const params = new URLSearchParams();
    if (opts.limit !== undefined) params.set("limit", String(opts.limit));
    if (opts.since !== undefined) params.set("since", String(opts.since));
    if (opts.before !== undefined) params.set("before", String(opts.before));
    const query = params.size > 0 ? `?${params.toString()}` : "";
    return this.requestJson<MessageWithSender[]>(
      "GET",
      `/api/rooms/${encodeURIComponent(roomId)}/messages${query}`,
      undefined,
      { auth: true },
    );
  }
}

/** Parse a JSON response body, surfacing a useful error on garbage. */
export function parseJsonBody<T>(bodyText: string): T {
  if (bodyText.length === 0) return undefined as T;
  try {
    return JSON.parse(bodyText) as T;
  } catch {
    const snippet = bodyText.length > 120 ? `${bodyText.slice(0, 120)}…` : bodyText;
    throw new ApiError(0, `server returned non-JSON body: ${snippet}`);
  }
}
