// `msgHash` vectors: SHA-256 over the *trimmed* content, lowercase hex.

import XCTest
@testable import PocketSkynetClient

final class MsgHashTests: XCTestCase {
    func testPlaintextVectors() {
        let vectors = Vectors.all.msgHash.plaintext
        XCTAssertFalse(vectors.isEmpty)
        for vector in vectors {
            XCTAssertEqual(msgHashPlaintext(vector.content), vector.msgHashHex,
                           "content \(vector.content.debugDescription)")
        }
    }

    func testTrimmingMatchesTheServer() {
        // "  hello \n" hashes as "hello" — the server stores the trimmed
        // string, so the hash covers what gets stored.
        guard let vector = Vectors.all.msgHash.plaintext.first(where: { $0.trimmedTo != nil }) else {
            return XCTFail("no trimmed vector")
        }
        XCTAssertEqual(msgHashPlaintext(vector.content),
                       msgHashPlaintext(vector.trimmedTo!))
    }

    func testUnicodeContentHashesItsUTF8Bytes() {
        // The Korean + emoji vector pins UTF-8 byte hashing.
        guard let vector = Vectors.all.msgHash.plaintext.first(where: { $0.content.contains("한글") }) else {
            return XCTFail("no unicode vector")
        }
        XCTAssertEqual(msgHashPlaintext(vector.content), vector.msgHashHex)
    }

    func testHashIsLowercaseHex64() {
        let hash = msgHashPlaintext("anything")
        XCTAssertEqual(hash.count, 64)
        XCTAssertEqual(hash, hash.lowercased())
        XCTAssertTrue(hash.allSatisfy { "0123456789abcdef".contains($0) })
    }
}
