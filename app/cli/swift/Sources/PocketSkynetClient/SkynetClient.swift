// The PocketSkynet API client.
//
// Endpoint shapes follow `app/docs/API.md`; the login flow follows the wire
// reality of `server/src/routes/auth.rs`:
//   challenge → EIP-191 personal_sign of the challenge string *verbatim* →
//   login {walletAddress, challengeId, signature[, username]} → Bearer JWT.
// A challenge is burned by success AND failure, so every retry starts with a
// fresh challenge. First-time logins need a username; `login(wallet:)` retries
// once with a generated one when the server says so.

import CryptoKit
import Foundation

public enum APIError: Error, CustomStringConvertible {
    /// Non-2xx status; the envelope is decoded when the body carried one.
    case http(status: Int, envelope: ErrorEnvelope?, body: String)
    case decoding(String)

    public var description: String {
        switch self {
        case .http(let status, let envelope, let body):
            if let message = envelope?.message {
                var text = "HTTP \(status): \(message)"
                if let errors = envelope?.errors, !errors.isEmpty {
                    text += " (\(errors.joined(separator: "; ")))"
                }
                if let code = envelope?.code {
                    text += " [\(code)]"
                }
                return text
            }
            return "HTTP \(status): \(body)"
        case .decoding(let detail):
            return "could not decode response: \(detail)"
        }
    }

    public var status: Int? {
        if case .http(let status, _, _) = self { return status }
        return nil
    }

    public var message: String? {
        if case .http(_, let envelope, _) = self { return envelope?.message }
        return nil
    }
}

/// `msgHash` for a plaintext message: SHA-256 of the *trimmed* content,
/// lowercase hex — the server stores the trimmed string, so the hash must
/// cover what gets stored, not what was typed.
public func msgHashPlaintext(_ content: String) -> String {
    let trimmed = content.trimmingCharacters(in: .whitespacesAndNewlines)
    let digest = SHA256.hash(data: Data(trimmed.utf8))
    return digest.map { String(format: "%02x", $0) }.joined()
}

public final class SkynetClient {
    public let baseURL: URL
    public let transport: Transport
    /// Bearer JWT once logged in.
    public var token: String?

    public init(baseURL: URL, transport: Transport = URLSessionTransport()) {
        self.baseURL = baseURL
        self.transport = transport
    }

    public convenience init(baseURL: URL, insecure: Bool, http3: Bool) {
        self.init(baseURL: baseURL, transport: URLSessionTransport(insecure: insecure, http3: http3))
    }

    // MARK: Raw request plumbing

    @discardableResult
    public func request(_ method: String, _ path: String, body: Data? = nil) async throws -> TransportResponse {
        guard let url = URL(string: baseURL.absoluteString + path) else {
            throw APIError.decoding("bad path: \(path)")
        }
        var headers = ["Accept": "application/json"]
        if body != nil {
            headers["Content-Type"] = "application/json"
        }
        if let token {
            headers["Authorization"] = "Bearer \(token)"
        }
        return try await transport.send(method: method, url: url, headers: headers, body: body)
    }

    private func call<Response: Decodable>(
        _ method: String, _ path: String, body: (some Encodable)? = Optional<Int>.none
    ) async throws -> Response {
        var encoded: Data?
        if let body {
            encoded = try JSONEncoder().encode(body)
        }
        let response = try await request(method, path, body: encoded)
        let text = String(data: response.body, encoding: .utf8) ?? ""
        guard (200..<300).contains(response.status) else {
            let envelope = try? JSONDecoder().decode(ErrorEnvelope.self, from: response.body)
            throw APIError.http(status: response.status, envelope: envelope, body: text)
        }
        do {
            return try JSONDecoder().decode(Response.self, from: response.body)
        } catch {
            throw APIError.decoding("\(error) — body: \(text)")
        }
    }

    // MARK: Endpoints

    public func health() async throws -> HealthResponse {
        try await call("GET", "/api/health")
    }

    public func serverInfo() async throws -> ServerInfo {
        try await call("GET", "/api/server/info")
    }

    public func challenge(walletAddress: String) async throws -> ChallengeResponse {
        try await call("POST", "/api/auth/challenge", body: ChallengeRequest(walletAddress: walletAddress))
    }

    public func login(_ request: LoginRequest) async throws -> LoginResponse {
        let response: LoginResponse = try await call("POST", "/api/auth/login", body: request)
        token = response.token
        return response
    }

    /// The whole login flow. Signs the challenge string exactly as received.
    ///
    /// With `username: nil`, a returning account reuses its stored username;
    /// a first-time account gets `400 Username is required for first-time
    /// login`, upon which this retries once — with a *fresh* challenge, since
    /// the failed attempt burned the first one — using a deterministic
    /// generated username.
    @discardableResult
    public func login(wallet: EthereumWallet, username: String? = nil) async throws -> LoginResponse {
        do {
            return try await attemptLogin(wallet: wallet, username: username)
        } catch let error as APIError
            where username == nil && error.status == 400
            && error.message == "Username is required for first-time login" {
            return try await attemptLogin(wallet: wallet, username: Self.generatedUsername(for: wallet.address))
        }
    }

    private func attemptLogin(wallet: EthereumWallet, username: String?) async throws -> LoginResponse {
        let challenge = try await challenge(walletAddress: wallet.address)
        let signature = try wallet.personalSign(message: challenge.message)
        return try await login(LoginRequest(
            walletAddress: wallet.address,
            challengeId: challenge.challengeId,
            signature: signature,
            username: username
        ))
    }

    /// Deterministic first-login username: valid under the server's username
    /// schema (3–100 chars, none of ``<>{};"'`\,``), unique per wallet.
    public static func generatedUsername(for address: String) -> String {
        "swift_" + address.dropFirst(2).prefix(10)
    }

    public func rooms() async throws -> [Room] {
        try await call("GET", "/api/rooms")
    }

    public func room(id: String) async throws -> Room {
        try await call("GET", "/api/rooms/\(id)")
    }

    public func createRoom(name: String, description: String? = nil) async throws -> Room {
        try await call("POST", "/api/rooms", body: CreateRoomRequest(name: name, description: description))
    }

    /// Send a plaintext message; `msgHash` is computed here.
    @discardableResult
    public func sendMessage(roomId: String, content: String) async throws -> Message {
        try await call("POST", "/api/rooms/\(roomId)/messages",
                       body: SendMessageRequest(content: content, msgHash: msgHashPlaintext(content)))
    }

    /// List messages, ascending by `(messageTimestamp, msgSerial)`.
    public func messages(roomId: String, limit: Int? = nil, before: Int64? = nil, since: Int64? = nil) async throws -> [Message] {
        var query: [String] = []
        if let limit { query.append("limit=\(limit)") }
        if let before { query.append("before=\(before)") }
        if let since { query.append("since=\(since)") }
        let suffix = query.isEmpty ? "" : "?" + query.joined(separator: "&")
        return try await call("GET", "/api/rooms/\(roomId)/messages\(suffix)")
    }

    public func profile() async throws -> User {
        try await call("GET", "/api/auth/profile")
    }
}
