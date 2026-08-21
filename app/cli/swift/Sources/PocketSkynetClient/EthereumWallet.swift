// Ethereum wallet signing: secp256k1 (RFC 6979 deterministic, low-S) with
// public-key recovery, and EIP-191 `personal_sign` — the flow the PocketSkynet
// login challenge requires.

import Foundation
import secp256k1

public enum WalletError: Error, CustomStringConvertible, Equatable {
    case malformedPrivateKey(String)
    case invalidScalar
    case malformedSignature(String)

    public var description: String {
        switch self {
        case .malformedPrivateKey(let detail): return "malformed private key: \(detail)"
        case .invalidScalar: return "private key is not a valid secp256k1 scalar"
        case .malformedSignature(let detail): return "malformed signature: \(detail)"
        }
    }
}

public struct EthereumWallet {
    private let privateKey: secp256k1.Recovery.PrivateKey

    /// Lowercase `0x…` address — the only form the API accepts or returns.
    public let address: String

    /// Import a 32-byte private key from hex (`0x`-prefixed or bare).
    public init(privateKeyHex: String) throws {
        guard let bytes = Hex.decode(privateKeyHex) else {
            throw WalletError.malformedPrivateKey("not hex")
        }
        guard bytes.count == 32 else {
            throw WalletError.malformedPrivateKey("expected 32 bytes, got \(bytes.count)")
        }
        try self.init(privateKeyBytes: bytes)
    }

    public init(privateKeyBytes: [UInt8]) throws {
        guard privateKeyBytes.count == 32 else {
            throw WalletError.malformedPrivateKey("expected 32 bytes, got \(privateKeyBytes.count)")
        }
        do {
            self.privateKey = try secp256k1.Recovery.PrivateKey(
                dataRepresentation: Data(privateKeyBytes), format: .uncompressed)
        } catch {
            // Zero, or >= the group order — libsecp256k1 refuses the scalar.
            throw WalletError.invalidScalar
        }
        self.address = Self.address(ofUncompressedPublicKey: [UInt8](privateKey.publicKey.dataRepresentation))
    }

    /// A fresh random wallet.
    public static func random() throws -> EthereumWallet {
        let key = try secp256k1.Recovery.PrivateKey(format: .uncompressed)
        return try EthereumWallet(privateKeyBytes: [UInt8](key.dataRepresentation))
    }

    /// keccak256(pubkey minus the 0x04 tag), last 20 bytes, lowercase hex.
    static func address(ofUncompressedPublicKey pub: [UInt8]) -> String {
        precondition(pub.count == 65 && pub[0] == 0x04, "expected an uncompressed secp256k1 point")
        let digest = Keccak256.hash(Array(pub[1...]))
        return "0x" + Hex.encode(Array(digest[12...]))
    }

    // MARK: EIP-191

    /// keccak256(0x19 ‖ "Ethereum Signed Message:\n" ‖ decimal(utf8 byte length) ‖ message).
    /// The length is the UTF-8 *byte* count, not the character count.
    public static func eip191Digest(message: String) -> [UInt8] {
        let body = Array(message.utf8)
        var preimage = Array("\u{19}Ethereum Signed Message:\n\(body.count)".utf8)
        preimage.append(contentsOf: body)
        return Keccak256.hash(preimage)
    }

    /// EIP-191 `personal_sign`: `0x` + 130 lowercase hex (r ‖ s ‖ v, v = recid + 27).
    /// RFC 6979 deterministic nonce, low-S normalized — the same bytes every time.
    public func personalSign(message: String) throws -> String {
        let digest = Self.eip191Digest(message: message)
        let signature = try privateKey.signature(for: HashDigest(digest))
        let compact = try signature.compactRepresentation
        var wire = [UInt8](compact.signature) // r ‖ s, 64 bytes
        wire.append(UInt8(compact.recoveryId) + 27)
        return "0x" + Hex.encode(wire)
    }

    /// Recover the lowercase signer address from an EIP-191 signature,
    /// or throw for a malformed one.
    public static func recoverAddress(message: String, signature: String) throws -> String {
        guard let bytes = Hex.decode(signature) else {
            throw WalletError.malformedSignature("not hex")
        }
        guard bytes.count == 65 else {
            throw WalletError.malformedSignature("expected 65 bytes, got \(bytes.count)")
        }
        let v = bytes[64]
        guard v == 27 || v == 28 else {
            throw WalletError.malformedSignature("v must be 27 or 28, got \(v)")
        }
        let digest = eip191Digest(message: message)
        do {
            let parsed = try secp256k1.Recovery.ECDSASignature(
                compactRepresentation: Data(bytes[0..<64]), recoveryId: Int32(v) - 27)
            let publicKey = try secp256k1.Recovery.PublicKey(
                HashDigest(digest), signature: parsed, format: .uncompressed)
            return address(ofUncompressedPublicKey: [UInt8](publicKey.dataRepresentation))
        } catch let error as WalletError {
            throw error
        } catch {
            throw WalletError.malformedSignature("recovery failed")
        }
    }
}
