// URL-construction safety: base-URL normalization and path-segment encoding
// so a caller-supplied roomId cannot retarget the request.

import XCTest
@testable import PocketSkynetClient

final class URLSafetyTests: XCTestCase {
    func testTrailingSlashOnBaseURLIsStripped() {
        let client = SkynetClient(baseURL: URL(string: "http://host:9099/")!)
        XCTAssertEqual(client.baseURL.absoluteString, "http://host:9099",
                       "a trailing slash must not yield //api/...")
    }

    func testMultipleTrailingSlashesAreStripped() {
        let client = SkynetClient(baseURL: URL(string: "http://host:9099///")!)
        XCTAssertEqual(client.baseURL.absoluteString, "http://host:9099")
    }

    func testNoTrailingSlashIsUnchanged() {
        let client = SkynetClient(baseURL: URL(string: "https://host:9101")!)
        XCTAssertEqual(client.baseURL.absoluteString, "https://host:9101")
    }

    func testServerRoomIdsPassThroughUnchanged() throws {
        // The server's own ids are [a-zA-Z0-9_.-] — none of which is encoded.
        for id in [
            "room_1749652739650_304e0eaf-bcf9-4682-a6a0-69bee8e40b97",
            "room_note_0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
            "room_lobby_0xabc.def-123",
        ] {
            XCTAssertEqual(try SkynetClient.pathSegment(id), id, "\(id) must pass through")
        }
    }

    func testTraversalAndDelimitersAreEncodedNotPassed() throws {
        // The attack the encoding closes: a value that would otherwise walk out
        // of /api/rooms/ and carry the bearer token elsewhere.
        let escape = try SkynetClient.pathSegment("x/../../auth/profile?")
        XCTAssertFalse(escape.contains("/"), "slashes must be encoded: \(escape)")
        XCTAssertFalse(escape.contains("?"), "query delimiter must be encoded: \(escape)")

        XCTAssertFalse(try SkynetClient.pathSegment("a#b").contains("#"))
        XCTAssertFalse(try SkynetClient.pathSegment("a?b").contains("?"))
        XCTAssertTrue(try SkynetClient.pathSegment("a b").contains("%20"))
    }
}
