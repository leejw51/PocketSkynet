// Loader for the cross-language protocol vectors in
// `app/core/tests/vectors/protocol-v1.json`, resolved by walking up from this
// file — robust to the checkout being a plain repo, a worktree, or an
// absorbed submodule, as long as the vectors travel with the sources.

import Foundation
import XCTest

struct Eip191Vector: Decodable {
    let name: String
    let message: String
    let messageUtf8Len: Int
    let privateKeyHex: String
    let address: String
    let digestHex: String
    let signatureHex: String
}

struct PrivateKeyImportVector: Decodable {
    let address: String
    let addressChecksummed: String
    let privateKeyHex: String
    let publicKeyUncompressedHex: String
}

struct AccountVector: Decodable {
    let address: String
    let addressChecksummed: String
    let privateKeyHex: String
}

struct WalletVectors: Decodable {
    let privateKeyImports: [PrivateKeyImportVector]
    let accounts: [AccountVector]
}

struct PlaintextHashVector: Decodable {
    let content: String
    let msgHashHex: String
    let trimmedTo: String?
}

struct EncryptedHashVector: Decodable {
    let ciphertextBase64: String
    let msgHashHex: String
}

struct MsgHashVectors: Decodable {
    let plaintext: [PlaintextHashVector]
    let encrypted: [EncryptedHashVector]
}

struct ProtocolVectors: Decodable {
    let formatVersion: Int
    let eip191: [Eip191Vector]
    let wallet: WalletVectors
    let msgHash: MsgHashVectors
}

enum Vectors {
    static let all: ProtocolVectors = {
        let url = locate()
        do {
            let data = try Data(contentsOf: url)
            return try JSONDecoder().decode(ProtocolVectors.self, from: data)
        } catch {
            fatalError("could not load protocol vectors at \(url.path): \(error)")
        }
    }()

    /// Walk up from this source file until `app/core/tests/vectors/protocol-v1.json`
    /// appears under some ancestor.
    static func locate() -> URL {
        var dir = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
        for _ in 0..<12 {
            let candidate = dir.appendingPathComponent("app/core/tests/vectors/protocol-v1.json")
            if FileManager.default.fileExists(atPath: candidate.path) {
                return candidate
            }
            let parent = dir.deletingLastPathComponent()
            if parent.path == dir.path { break }
            dir = parent
        }
        fatalError("protocol-v1.json not found above \(#filePath) — is the repo layout intact?")
    }
}
