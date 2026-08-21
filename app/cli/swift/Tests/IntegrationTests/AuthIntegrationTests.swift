// Login flow against a real server: happy path, first-time username retry,
// wrong signature, challenge burn-on-failure and replay, JWT tampering.

import Foundation
import PocketSkynetClient
import XCTest

final class AuthIntegrationTests: XCTestCase {
    static var server: TestServer!

    override class func setUp() {
        super.setUp()
        server = TestServer.start()
    }

    override class func tearDown() {
        server?.stop()
        server = nil
        super.tearDown()
    }

    var server: TestServer { Self.server }

    func testLoginHappyPathWithExplicitUsername() async throws {
        let wallet = try EthereumWallet.random()
        let client = server.client()
        let response = try await client.login(wallet: wallet, username: "swift_alice")
        XCTAssertEqual(response.user.walletAddress, wallet.address)
        XCTAssertEqual(response.user.username, "swift_alice")
        XCTAssertFalse(response.token.isEmpty)
        // The token actually works.
        let profile = try await client.profile()
        XCTAssertEqual(profile.walletAddress, wallet.address)
    }

    func testFirstTimeLoginWithoutUsernameRetriesWithGeneratedOne() async throws {
        // A fresh wallet with no username: the first login attempt fails with
        // 400 "Username is required for first-time login" (burning the
        // challenge), and the client retries once with a fresh challenge and
        // a deterministic username.
        let wallet = try EthereumWallet.random()
        let client = server.client()
        let response = try await client.login(wallet: wallet)
        XCTAssertEqual(response.user.username, SkynetClient.generatedUsername(for: wallet.address))

        // A returning login without a username reuses the stored one — no retry.
        let again = server.client()
        let second = try await again.login(wallet: wallet)
        XCTAssertEqual(second.user.username, response.user.username)
    }

    func testBareLoginWithoutUsernameFailsFirstTime() async throws {
        // The raw endpoint behavior the retry is built on.
        let wallet = try EthereumWallet.random()
        let client = server.client()
        let challenge = try await client.challenge(walletAddress: wallet.address)
        let signature = try wallet.personalSign(message: challenge.message)
        do {
            _ = try await client.login(LoginRequest(
                walletAddress: wallet.address, challengeId: challenge.challengeId, signature: signature))
            XCTFail("first-time login without a username must fail")
        } catch let error as APIError {
            XCTAssertEqual(error.status, 400)
            XCTAssertEqual(error.message, "Username is required for first-time login")
        }
    }

    func testWrongSignatureIs401() async throws {
        let wallet = try EthereumWallet.random()
        let impostor = try EthereumWallet.random()
        let client = server.client()
        let challenge = try await client.challenge(walletAddress: wallet.address)
        // A perfectly well-formed signature — by the wrong key.
        let signature = try impostor.personalSign(message: challenge.message)
        do {
            _ = try await client.login(LoginRequest(
                walletAddress: wallet.address, challengeId: challenge.challengeId,
                signature: signature, username: "swift_mallory"))
            XCTFail("wrong-key signature must be refused")
        } catch let error as APIError {
            XCTAssertEqual(error.status, 401)
            XCTAssertEqual(error.message, "Invalid signature")
        }
    }

    func testChallengeReplayIs400() async throws {
        // A challenge is consumed atomically by a *successful* login; replaying
        // it must fail.
        let wallet = try EthereumWallet.random()
        let client = server.client()
        let challenge = try await client.challenge(walletAddress: wallet.address)
        let signature = try wallet.personalSign(message: challenge.message)
        _ = try await client.login(LoginRequest(
            walletAddress: wallet.address, challengeId: challenge.challengeId,
            signature: signature, username: "swift_replayer"))

        do {
            _ = try await client.login(LoginRequest(
                walletAddress: wallet.address, challengeId: challenge.challengeId,
                signature: signature, username: "swift_replayer"))
            XCTFail("challenge replay must be refused")
        } catch let error as APIError {
            XCTAssertEqual(error.status, 400)
            XCTAssertEqual(error.message, "Invalid or expired challenge")
        }
    }

    func testFailedLoginBurnsTheChallengeToo() async throws {
        // A challenge is burned by failure as well as success: after a wrong
        // signature, retrying with the *same* challenge and the right key
        // still fails. Clients must fetch a fresh challenge per attempt.
        let wallet = try EthereumWallet.random()
        let impostor = try EthereumWallet.random()
        let client = server.client()
        let challenge = try await client.challenge(walletAddress: wallet.address)

        _ = try? await client.login(LoginRequest(
            walletAddress: wallet.address, challengeId: challenge.challengeId,
            signature: try impostor.personalSign(message: challenge.message),
            username: "swift_burner"))

        do {
            _ = try await client.login(LoginRequest(
                walletAddress: wallet.address, challengeId: challenge.challengeId,
                signature: try wallet.personalSign(message: challenge.message),
                username: "swift_burner"))
            XCTFail("the burned challenge must not be reusable")
        } catch let error as APIError {
            XCTAssertEqual(error.status, 400)
            XCTAssertEqual(error.message, "Invalid or expired challenge")
        }
    }

    func testTamperedJWTIs401() async throws {
        let (client, _, response) = try await server.loginFreshUser()
        // Flip a character inside the signature segment.
        var token = response.token
        let last = token.removeLast()
        token.append(last == "A" ? "B" : "A")
        client.token = token
        do {
            _ = try await client.rooms()
            XCTFail("a tampered token must be refused")
        } catch let error as APIError {
            XCTAssertEqual(error.status, 401)
            XCTAssertEqual(error.message, "Invalid token")
        }
    }

    func testAbsentJWTIs401() async throws {
        let client = server.client() // never logged in
        do {
            _ = try await client.rooms()
            XCTFail("no token must be refused")
        } catch let error as APIError {
            XCTAssertEqual(error.status, 401)
            XCTAssertEqual(error.message, "No token provided")
        }
    }

    func testHealthNeedsNoAuth() async throws {
        let health = try await server.client().health()
        XCTAssertEqual(health.status, "ok")
    }
}
