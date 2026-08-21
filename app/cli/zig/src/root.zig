//! `pocketskynet` — a Zig client library for the PocketSkynet server.
//!
//! Layers, bottom to top:
//! - `hex`      lowercase hex helpers
//! - `secp`     secp256k1 signing (RFC 6979, low-S, recovery id), addresses,
//!              EIP-55 — pure Zig on `std.crypto.ecc.Secp256k1`
//! - `eip191`   EIP-191 `personal_sign`
//! - `msghash`  plaintext `msgHash` (SHA-256 of trimmed content)
//! - `transport` HTTP/1.1(+TLS) via `std.http.Client`, HTTP/3 and
//!              `--insecure` HTTPS via an exec-style curl subprocess
//! - `api`      the typed API client (login flow, rooms, messages, health)

pub const hex = @import("hex.zig");
pub const secp = @import("secp.zig");
pub const eip191 = @import("eip191.zig");
pub const msghash = @import("msghash.zig");
pub const transport = @import("transport.zig");
pub const api = @import("api.zig");

test {
    @import("std").testing.refAllDecls(@This());
}
