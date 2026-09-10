// HTTP/3 end to end. URLSession on macOS is backed by Network.framework and
// negotiates QUIC natively when a request is marked `assumesHTTP3Capable`;
// `GET /api/server/info` reports which protocol carried the request, which is
// the only honest way to know the upgrade really happened.
//
// If this macOS cannot negotiate h3 (older Network.framework), the suite
// skips with a clear message rather than passing vacuously.

import Foundation
import PocketSkynetClient
import XCTest

final class HTTP3IntegrationTests: XCTestCase {
    static var server: TestServer!

    override class func setUp() {
        super.setUp()
        server = TestServer.start(http3: true)
    }

    override class func tearDown() {
        server?.stop()
        server = nil
        super.tearDown()
    }

    var server: TestServer { Self.server }

    /// The QUIC client, or an XCTSkip when this host cannot speak h3 at all.
    private func h3Client() async throws -> SkynetClient {
        let client = server.http3Client()
        do {
            _ = try await client.health()
        } catch let error where !(error is APIError) {
            throw XCTSkip("""
                URLSession could not reach the QUIC listener at \(server.http3URL) — \
                HTTP/3 appears unavailable on this macOS: \(error)
                """)
        }
        return client
    }

    func testServerConfirmsH3CarriedTheRequest() async throws {
        let client = try await h3Client()
        let info = try await client.serverInfo()
        XCTAssertEqual(info.protocolName, "h3",
                       "the server saw \(info.protocolName), not QUIC — the transport lied")
    }

    func testTCPListenerReportsNotH3ForContrast() async throws {
        // The control: the same endpoint over the TCP listener must *not*
        // claim h3, or the assertion above proves nothing.
        let info = try await server.client().serverInfo()
        XCTAssertNotEqual(info.protocolName, "h3")
    }

    func testWholeFlowOverH3() async throws {
        let client = try await h3Client()
        let wallet = try EthereumWallet.random()
        let login = try await client.login(wallet: wallet, username: "swift_quic")
        XCTAssertEqual(login.user.walletAddress, wallet.address)

        let room = try await client.createRoom(name: "quic room")
        let sent = try await client.sendMessage(roomId: room.id, content: "over quic")
        XCTAssertEqual(sent.content, "over quic")

        let messages = try await client.messages(roomId: room.id)
        XCTAssertEqual(messages.last?.content, "over quic")

        // Both listeners serve the same data: the message sent over QUIC is
        // visible over TCP with the same token.
        let tcp = server.client()
        tcp.token = client.token
        let viaTCP = try await tcp.messages(roomId: room.id)
        XCTAssertEqual(viaTCP.last?.content, "over quic")
    }
}
