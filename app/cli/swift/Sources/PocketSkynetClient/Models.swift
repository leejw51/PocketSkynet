// Wire types — exact camelCase field names from `app/docs/API.md`.
//
// Decoding is tolerant of unknown fields (Codable's default) and of the
// server's `undefined`-is-omitted convention: everything the spec marks
// nullable or omissible is optional here.

import Foundation

// MARK: - Requests

public struct ChallengeRequest: Codable, Equatable {
    public var walletAddress: String
    public init(walletAddress: String) { self.walletAddress = walletAddress }
}

public struct LoginRequest: Codable, Equatable {
    public var walletAddress: String
    public var challengeId: String
    public var signature: String
    /// Omitted from the JSON entirely when `nil` — a returning login reuses
    /// the stored username, a first-time login must carry one.
    public var username: String?

    public init(walletAddress: String, challengeId: String, signature: String, username: String? = nil) {
        self.walletAddress = walletAddress
        self.challengeId = challengeId
        self.signature = signature
        self.username = username
    }
}

public struct CreateRoomRequest: Codable, Equatable {
    public var name: String
    public var description: String?
    public init(name: String, description: String? = nil) {
        self.name = name
        self.description = description
    }
}

public struct SendMessageRequest: Codable, Equatable {
    public var content: String
    /// SHA-256 of the *trimmed* content, lowercase hex.
    public var msgHash: String
    public init(content: String, msgHash: String) {
        self.content = content
        self.msgHash = msgHash
    }
}

// MARK: - Responses

public struct ChallengeResponse: Codable, Equatable {
    public let challengeId: String
    public let message: String
    public let expiresAt: String
}

public struct User: Codable, Equatable {
    public let walletAddress: String
    public let username: String
    public let publicKey: String?
    public let publicKeySig: String?
    public let profileImage: String?
    public let createdAt: String?
    public let updatedAt: String?
}

public struct LoginResponse: Codable, Equatable {
    public let user: User
    public let token: String
    public let fruitnationWallet: String?
    public let encryptionSalt: String?
}

public struct Room: Codable, Equatable {
    public let id: String
    public let name: String
    public let description: String?
    public let kind: String?
    public let currentKeyVersion: Int?
    public let keyRotationPending: Bool?
    public let createdAt: String?
    // Enrichment on RoomWithMembers; absent on a bare Room.
    public let memberCount: Int?
    public let admins: [User]?
    public let hasEncryption: Bool?
    public let lastMessage: Message?
    public let unreadCount: Int?
    public let lastReadSerial: Int64?
}

public final class Message: Codable, Equatable {
    public let id: String
    public let roomId: String
    public let senderAddress: String
    public let content: String
    public let msgHash: String?
    public let messageTimestamp: Int64
    public let msgType: String
    public let msgSerial: Int64
    public let isDeleted: Bool
    public let editedAt: String?
    public let createdAt: String?
    public let isEncrypted: Bool?
    public let iv: String?
    public let hmac: String?
    public let encVer: Int?
    public let keyVersion: Int?
    public let txHash: String?
    public let targetMessageId: String?
    public let emoticonCode: String?
    public let sender: User?
    public let replyCount: Int?
    public let lastReplyAt: Int64?

    public static func == (lhs: Message, rhs: Message) -> Bool {
        lhs.id == rhs.id && lhs.msgSerial == rhs.msgSerial && lhs.content == rhs.content
    }
}

public struct HealthResponse: Codable, Equatable {
    public let status: String
    public let uptime: Int?
}

public struct ServerInfo: Codable, Equatable {
    /// What carried *this* request: `"h3"`, `"h2"`, or `"http/1.1"` — the only
    /// honest way for a client to know it really spoke QUIC.
    public let protocolName: String
    public let scheme: String?
    public let port: Int?
    public let http3Port: Int?
    public let http3Available: Bool?
    public let uptime: Int?

    enum CodingKeys: String, CodingKey {
        case protocolName = "protocol"
        case scheme, port, http3Port, http3Available, uptime
    }
}

// MARK: - Error envelope

/// The three §1.5 error shapes: `{message}`, `{message, errors[]}`, and
/// `{code, message, …}`. All fields optional so any of them decodes.
public struct ErrorEnvelope: Codable, Equatable {
    public let message: String?
    public let errors: [String]?
    public let code: String?
    public let currentKeyVersion: Int?

    public init(message: String? = nil, errors: [String]? = nil,
                code: String? = nil, currentKeyVersion: Int? = nil) {
        self.message = message
        self.errors = errors
        self.code = code
        self.currentKeyVersion = currentKeyVersion
    }
}
