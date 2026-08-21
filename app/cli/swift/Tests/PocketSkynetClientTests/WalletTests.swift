// Key import and address derivation against `wallet.privateKeyImports` and
// `wallet.accounts`, plus malformed-key rejection.

import XCTest
@testable import PocketSkynetClient

final class WalletTests: XCTestCase {
    func testPrivateKeyImportsDeriveTheVectorAddresses() throws {
        let vectors = Vectors.all.wallet.privateKeyImports
        XCTAssertFalse(vectors.isEmpty)
        for vector in vectors {
            let wallet = try EthereumWallet(privateKeyHex: vector.privateKeyHex)
            XCTAssertEqual(wallet.address, vector.address, "import \(vector.address)")
            XCTAssertEqual(wallet.address, vector.addressChecksummed.lowercased(),
                           "vector self-consistency")
        }
    }

    func testAccountsKeyToAddress() throws {
        // The BIP-39 derivation itself is out of scope; each account vector
        // carries its private key, and key → address must hold for every one.
        let accounts = Vectors.all.wallet.accounts
        XCTAssertFalse(accounts.isEmpty)
        for account in accounts {
            let wallet = try EthereumWallet(privateKeyHex: account.privateKeyHex)
            XCTAssertEqual(wallet.address, account.address, "account \(account.address)")
        }
    }

    func testImportAcceptsBareAndPrefixedHex() throws {
        let prefixed = try EthereumWallet(
            privateKeyHex: "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
        let bare = try EthereumWallet(
            privateKeyHex: "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
        XCTAssertEqual(prefixed.address, bare.address)
    }

    func testMalformedKeysAreRejected() {
        // Not hex at all.
        XCTAssertThrowsError(try EthereumWallet(privateKeyHex: "not-a-key"))
        // Odd length.
        XCTAssertThrowsError(try EthereumWallet(privateKeyHex: "0xabc"))
        // Wrong byte count (31 and 33 bytes).
        XCTAssertThrowsError(try EthereumWallet(privateKeyHex: "0x" + String(repeating: "ab", count: 31)))
        XCTAssertThrowsError(try EthereumWallet(privateKeyHex: "0x" + String(repeating: "ab", count: 33)))
        // Empty.
        XCTAssertThrowsError(try EthereumWallet(privateKeyHex: ""))
        // The zero scalar is not a valid secp256k1 key.
        XCTAssertThrowsError(try EthereumWallet(privateKeyHex: "0x" + String(repeating: "00", count: 32)))
        // The group order n is out of range (valid scalars are 1..n-1).
        XCTAssertThrowsError(try EthereumWallet(
            privateKeyHex: "0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141"))
        // As is anything above it.
        XCTAssertThrowsError(try EthereumWallet(privateKeyHex: "0x" + String(repeating: "ff", count: 32)))
    }

    func testBoundaryScalarsAreValid() throws {
        // 1 and n-1 are both valid keys.
        _ = try EthereumWallet(privateKeyHex: "0x" + String(repeating: "00", count: 31) + "01")
        _ = try EthereumWallet(
            privateKeyHex: "0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140")
    }

    func testRandomWalletsAreDistinctAndWellFormed() throws {
        let a = try EthereumWallet.random()
        let b = try EthereumWallet.random()
        XCTAssertNotEqual(a.address, b.address)
        for wallet in [a, b] {
            XCTAssertTrue(wallet.address.hasPrefix("0x"))
            XCTAssertEqual(wallet.address.count, 42)
            XCTAssertEqual(wallet.address, wallet.address.lowercased())
        }
    }

    func testHexRoundTrip() {
        XCTAssertEqual(Hex.decode("0xDEADbeef"), [0xde, 0xad, 0xbe, 0xef])
        XCTAssertEqual(Hex.encode([0xde, 0xad, 0xbe, 0xef]), "deadbeef")
        XCTAssertNil(Hex.decode("0x123"))   // odd length
        XCTAssertNil(Hex.decode("zz"))      // not hex
        XCTAssertEqual(Hex.decode(""), [])  // empty is fine
    }
}
