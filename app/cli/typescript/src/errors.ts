/**
 * Error envelope handling. The server produces exactly three shapes:
 *
 *   { "message": "Access denied" }
 *   { "message": "Validation failed", "errors": ["roomId: …"] }
 *   { "code": "KEY_ROTATION_REQUIRED", "message": "…", "currentKeyVersion": 3 }
 */

export class ApiError extends Error {
  /** HTTP status. */
  readonly status: number;
  /** `errors` array from the validation envelope, when present. */
  readonly errors: string[] | undefined;
  /** Machine-readable `code`, only on the two 409 key-conflict envelopes. */
  readonly code: string | undefined;
  /** `currentKeyVersion` from a key-conflict envelope. */
  readonly currentKeyVersion: number | undefined;

  constructor(
    status: number,
    message: string,
    opts: { errors?: string[]; code?: string; currentKeyVersion?: number } = {},
  ) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.errors = opts.errors;
    this.code = opts.code;
    this.currentKeyVersion = opts.currentKeyVersion;
  }
}

/** A transport-level failure (connection refused, TLS refusal, curl error). */
export class TransportError extends Error {
  override readonly cause: unknown;

  constructor(message: string, cause?: unknown) {
    super(message);
    this.name = "TransportError";
    this.cause = cause;
  }
}

/**
 * Build an {@link ApiError} from a non-2xx response body. Tolerates all three
 * envelope shapes, non-JSON bodies and bodies with the wrong shape.
 */
export function apiErrorFromBody(status: number, bodyText: string): ApiError {
  let parsed: unknown;
  try {
    parsed = JSON.parse(bodyText);
  } catch {
    parsed = undefined;
  }
  if (typeof parsed === "object" && parsed !== null) {
    const obj = parsed as Record<string, unknown>;
    const message =
      typeof obj["message"] === "string" && obj["message"].length > 0
        ? obj["message"]
        : `HTTP ${status}`;
    const errors = Array.isArray(obj["errors"])
      ? obj["errors"].filter((e): e is string => typeof e === "string")
      : undefined;
    const code = typeof obj["code"] === "string" ? obj["code"] : undefined;
    const currentKeyVersion =
      typeof obj["currentKeyVersion"] === "number" ? obj["currentKeyVersion"] : undefined;
    const opts: { errors?: string[]; code?: string; currentKeyVersion?: number } = {};
    if (errors !== undefined) opts.errors = errors;
    if (code !== undefined) opts.code = code;
    if (currentKeyVersion !== undefined) opts.currentKeyVersion = currentKeyVersion;
    return new ApiError(status, message, opts);
  }
  const snippet = bodyText.length > 200 ? `${bodyText.slice(0, 200)}…` : bodyText;
  return new ApiError(status, snippet.trim().length > 0 ? snippet : `HTTP ${status}`);
}
