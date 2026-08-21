// EIP-191 `personal_sign` against every `eip191[]` protocol vector:
// digest, byte-exact signature, and address recovery — plus the wire-format
// invariants (low-S, v ∈ {27, 28}) the server's verifier depends on.

import XCTest
@testable import PocketSkynetClient

final class EIP191Tests: XCTestCase {
    /// secp256k1 group order n, big-endian.
    static let order: [UInt8] = Hex.decode(
        "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141")!
    /// n / 2 — the low-S boundary.
    static let halfOrder: [UInt8] = Hex.decode(
        "7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0")!

    /// Big-endian unsigned comparison: is a <= b?
    static func lessOrEqual(_ a: [UInt8], _ b: [UInt8]) -> Bool {
        precondition(a.count == b.count)
        for i in 0..<a.count {
            if a[i] != b[i] { return a[i] < b[i] }
        }
        return true
    }

    func testKeccakKnownAnswer() {
        // keccak256("") — the classic discriminator between Keccak and SHA-3.
        XCTAssertEqual(
            Hex.encode(Keccak256.hash([UInt8]())),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
        XCTAssertEqual(
            Hex.encode(Keccak256.hash(Array("abc".utf8))),
            "4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45")
        // A multi-block input (> 136-byte rate): keccak256 of 200 'a's.
        XCTAssertEqual(
            Hex.encode(Keccak256.hash([UInt8](repeating: 0x61, count: 200))),
            "96ea54061def936c4be90b518992fdc6f12f535068a256229aca54267b4d084d")
    }

    func testEveryEip191VectorDigest() {
        for vector in Vectors.all.eip191 {
            XCTAssertEqual(Array(vector.message.utf8).count, vector.messageUtf8Len,
                           "\(vector.name): vector self-check")
            XCTAssertEqual(Hex.encode(EthereumWallet.eip191Digest(message: vector.message)),
                           vector.digestHex, "\(vector.name): digest")
        }
    }

    func testEveryEip191VectorSignatureByteExact() throws {
        for vector in Vectors.all.eip191 {
            let wallet = try EthereumWallet(privateKeyHex: vector.privateKeyHex)
            XCTAssertEqual(wallet.address, vector.address, "\(vector.name): address")
            let signature = try wallet.personalSign(message: vector.message)
            XCTAssertEqual(signature, vector.signatureHex, "\(vector.name): signature")
            XCTAssertEqual(signature.count, 132, "\(vector.name): 0x + 130 hex")
            XCTAssertEqual(signature, signature.lowercased(), "\(vector.name): lowercase hex")
        }
    }

    func testEveryEip191VectorRecovery() throws {
        for vector in Vectors.all.eip191 {
            let recovered = try EthereumWallet.recoverAddress(
                message: vector.message, signature: vector.signatureHex)
            XCTAssertEqual(recovered, vector.address, "\(vector.name): recovery")
        }
    }

    func testUnicodeLengthIsBytesNotCharacters() {
        // "🍓 strawberry" is 12 characters but 15 UTF-8 bytes; the EIP-191
        // prefix must carry 15. Signing with the character count instead
        // must yield a *different* digest.
        guard let vector = Vectors.all.eip191.first(where: { $0.name == "unicode-length-is-bytes" }) else {
            return XCTFail("vector missing")
        }
        XCTAssertEqual(vector.message.count, 12)
        XCTAssertEqual(Array(vector.message.utf8).count, 15)
        XCTAssertEqual(Hex.encode(EthereumWallet.eip191Digest(message: vector.message)),
                       vector.digestHex)

        var wrongPreimage = Array("\u{19}Ethereum Signed Message:\n\(vector.message.count)".utf8)
        wrongPreimage.append(contentsOf: Array(vector.message.utf8))
        XCTAssertNotEqual(Hex.encode(Keccak256.hash(wrongPreimage)), vector.digestHex,
                          "character-count digest must differ")
    }

    func testSignaturesAreLowSWithCanonicalV() throws {
        // The vectors, plus fresh signatures over assorted messages: s must
        // stay in the low half of the group and v must be 27 or 28.
        for vector in Vectors.all.eip191 {
            let bytes = Hex.decode(vector.signatureHex)!
            let s = Array(bytes[32..<64])
            XCTAssertTrue(Self.lessOrEqual(s, Self.halfOrder), "\(vector.name): s must be low")
            XCTAssertTrue(bytes[64] == 27 || bytes[64] == 28, "\(vector.name): v must be 27/28")
        }
        let wallet = try EthereumWallet(
            privateKeyHex: "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
        for message in ["a", "low-s check", "메시지 🍓", String(repeating: "x", count: 500)] {
            let bytes = Hex.decode(try wallet.personalSign(message: message))!
            XCTAssertEqual(bytes.count, 65)
            let s = Array(bytes[32..<64])
            XCTAssertTrue(Self.lessOrEqual(s, Self.halfOrder), "low-S violated for \(message.prefix(12))")
            XCTAssertTrue(bytes[64] == 27 || bytes[64] == 28)
        }
    }

    func testSigningIsDeterministic() throws {
        // RFC 6979: same key + message → same signature, every time.
        let wallet = try EthereumWallet(
            privateKeyHex: "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        let first = try wallet.personalSign(message: "determinism")
        let second = try wallet.personalSign(message: "determinism")
        XCTAssertEqual(first, second)
    }

    func testRecoverRejectsMalformedSignatures() {
        // Not hex.
        XCTAssertThrowsError(try EthereumWallet.recoverAddress(message: "m", signature: "0xzz"))
        // Too short.
        XCTAssertThrowsError(try EthereumWallet.recoverAddress(message: "m", signature: "0x1234"))
        // Bad v (not 27/28).
        let sig64 = String(repeating: "11", count: 64)
        XCTAssertThrowsError(try EthereumWallet.recoverAddress(message: "m", signature: "0x" + sig64 + "00"))
    }

    func testRecoveryOfTamperedSignatureDoesNotYieldSigner() throws {
        let wallet = try EthereumWallet(
            privateKeyHex: "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
        var bytes = Hex.decode(try wallet.personalSign(message: "tamper me"))!
        bytes[10] ^= 0xff
        let tampered = "0x" + Hex.encode(bytes)
        // Either recovery fails outright, or it recovers some *other* address.
        if let recovered = try? EthereumWallet.recoverAddress(message: "tamper me", signature: tampered) {
            XCTAssertNotEqual(recovered, wallet.address)
        }
    }
}
