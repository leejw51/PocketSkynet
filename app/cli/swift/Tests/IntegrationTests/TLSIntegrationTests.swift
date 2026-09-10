// HTTPS with the server's self-signed certificate: the --insecure trust
// override must work, and its absence must refuse the connection.

import Foundation
import PocketSkynetClient
import XCTest

final class TLSIntegrationTests: XCTestCase {
    static var server: TestServer!

    override class func setUp() {
        super.setUp()
        server = TestServer.start(tls: true)
    }

    override class func tearDown() {
        server?.stop()
        server = nil
        super.tearDown()
    }

    var server: TestServer { Self.server }

    func testInsecureClientCompletesTheWholeFlowOverHTTPS() async throws {
        let client = SkynetClient(baseURL: URL(string: server.baseURL)!, insecure: true, http3: false)
        let wallet = try EthereumWallet.random()
        _ = try await client.login(wallet: wallet, username: "swift_tls")
        let room = try await client.createRoom(name: "tls room")
        _ = try await client.sendMessage(roomId: room.id, content: "over https")
        let messages = try await client.messages(roomId: room.id)
        XCTAssertEqual(messages.last?.content, "over https")
    }

    func testWithoutInsecureTheSelfSignedCertIsRefused() async throws {
        // Default trust evaluation must reject a certificate no real client
        // would accept — a request must fail at the transport level, never
        // reach the API.
        let client = SkynetClient(baseURL: URL(string: server.baseURL)!, insecure: false, http3: false)
        do {
            _ = try await client.health()
            XCTFail("a self-signed certificate must be refused without --insecure")
        } catch let error as APIError {
            XCTFail("the request must not reach the API, got \(error)")
        } catch {
            // Transport-level refusal — exactly right.
        }
    }
}
