// Rooms and messages against a real server: built-in room provisioning,
// create/list/invalid, send/list/limit/unicode ordering, the 403 no-oracle
// rule, msgHash validation, and one concurrency test.

import Foundation
import PocketSkynetClient
import XCTest

final class RoomsMessagesIntegrationTests: XCTestCase {
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

    // MARK: Rooms

    func testFreshAccountHasTheThreeBuiltinRooms() async throws {
        let (client, wallet, _) = try await server.loginFreshUser()
        let rooms = try await client.rooms()
        // Assert *membership*, never counts — a server upgrade may add rooms.
        for kind in ["note", "jarvis", "lobby"] {
            XCTAssertTrue(rooms.contains { $0.id == "room_\(kind)_\(wallet.address)" },
                          "missing built-in \(kind) room; got \(rooms.map(\.id))")
        }
        XCTAssertTrue(rooms.contains { $0.name == "My Note" })
        XCTAssertTrue(rooms.contains { $0.name == "My Jarvis" })
        XCTAssertTrue(rooms.contains { $0.name == "My Lobby" })
    }

    func testCreateRoomAndListIt() async throws {
        let (client, _, _) = try await server.loginFreshUser()
        let room = try await client.createRoom(name: "Swift Test Room", description: "made by XCTest")
        XCTAssertTrue(room.id.hasPrefix("room_"))
        XCTAssertEqual(room.name, "Swift Test Room")

        let rooms = try await client.rooms()
        XCTAssertTrue(rooms.contains { $0.id == room.id }, "created room must appear in the list")

        let fetched = try await client.room(id: room.id)
        XCTAssertEqual(fetched.id, room.id)
    }

    func testInvalidRoomNameIsValidationFailed() async throws {
        let (client, _, _) = try await server.loginFreshUser()
        do {
            _ = try await client.createRoom(name: "<script>alert(1)</script>")
            XCTFail("forbidden characters must be refused")
        } catch let error as APIError {
            XCTAssertEqual(error.status, 400)
            XCTAssertEqual(error.message, "Validation failed")
            guard case .http(_, let envelope, _) = error else { return XCTFail() }
            XCTAssertFalse(envelope?.errors?.isEmpty ?? true, "the envelope carries an errors array")
        }
    }

    // MARK: The 403 no-oracle rule

    func testForeignRoomIs403() async throws {
        let (alice, _, _) = try await server.loginFreshUser()
        let (bob, _, _) = try await server.loginFreshUser()
        let room = try await alice.createRoom(name: "Alice only")

        do {
            _ = try await bob.messages(roomId: room.id)
            XCTFail("a non-member must be refused")
        } catch let error as APIError {
            XCTAssertEqual(error.status, 403)
            XCTAssertEqual(error.message, "Access denied")
        }
    }

    func testNonexistentRoomIsAlso403NotAnOracle() async throws {
        let (client, _, _) = try await server.loginFreshUser()
        // Well-formed id that names nothing: same 403, same message — a
        // caller cannot distinguish "not yours" from "not there".
        do {
            _ = try await client.messages(roomId: "room_definitely_missing_123456")
            XCTFail("a nonexistent room must not 404")
        } catch let error as APIError {
            XCTAssertEqual(error.status, 403)
            XCTAssertEqual(error.message, "Access denied")
        }
        do {
            _ = try await client.sendMessage(roomId: "room_definitely_missing_123456", content: "hi")
            XCTFail("sending into a nonexistent room must not 404")
        } catch let error as APIError {
            XCTAssertEqual(error.status, 403)
        }
    }

    // MARK: Messages

    func testSendAndListMessages() async throws {
        let (client, _, _) = try await server.loginFreshUser()
        let room = try await client.createRoom(name: "chat")

        let first = try await client.sendMessage(roomId: room.id, content: "first")
        let second = try await client.sendMessage(roomId: room.id, content: "second")
        XCTAssertEqual(first.msgType, "add")
        XCTAssertEqual(first.content, "first")
        XCTAssertEqual(first.msgHash, msgHashPlaintext("first"))
        XCTAssertNotNil(first.sender, "POST returns MessageWithSender")

        let messages = try await client.messages(roomId: room.id)
        XCTAssertEqual(messages.map(\.content), ["first", "second"], "ascending order")
        XCTAssertLessThan(first.msgSerial, second.msgSerial)
        // Ordered by (messageTimestamp, msgSerial), ascending.
        let pairs = messages.map { ($0.messageTimestamp, $0.msgSerial) }
        for (earlier, later) in zip(pairs, pairs.dropFirst()) {
            XCTAssertTrue(earlier.0 < later.0 || (earlier.0 == later.0 && earlier.1 < later.1),
                          "list must ascend by (messageTimestamp, msgSerial)")
        }
    }

    func testMessageContentIsTrimmedAndHashCoversTheTrim() async throws {
        let (client, _, _) = try await server.loginFreshUser()
        let room = try await client.createRoom(name: "trim")
        let sent = try await client.sendMessage(roomId: room.id, content: "  hello \n")
        XCTAssertEqual(sent.content, "hello", "the server stores the trimmed string")
        XCTAssertEqual(sent.msgHash, msgHashPlaintext("hello"))
    }

    func testUnicodeMessageSurvivesTheRoundTrip() async throws {
        let (client, _, _) = try await server.loginFreshUser()
        let room = try await client.createRoom(name: "유니코드 🍓")
        let content = "한글 메시지 🍓🍊"
        _ = try await client.sendMessage(roomId: room.id, content: content)
        let messages = try await client.messages(roomId: room.id)
        XCTAssertEqual(messages.last?.content, content)
        XCTAssertEqual(messages.last?.msgHash, msgHashPlaintext(content))
    }

    func testMessageListLimit() async throws {
        let (client, _, _) = try await server.loginFreshUser()
        let room = try await client.createRoom(name: "limits")
        for i in 1...4 {
            _ = try await client.sendMessage(roomId: room.id, content: "message \(i)")
        }
        let limited = try await client.messages(roomId: room.id, limit: 2)
        // The newest N, still ascending.
        XCTAssertEqual(limited.map(\.content), ["message 3", "message 4"])
    }

    func testMsgHashValidation() async throws {
        let (client, _, _) = try await server.loginFreshUser()
        let room = try await client.createRoom(name: "hashes")

        // Missing msgHash → 400 Validation failed.
        let missing = try await client.request(
            "POST", "/api/rooms/\(room.id)/messages",
            body: Data(#"{"content":"hi"}"#.utf8))
        XCTAssertEqual(missing.status, 400, "msgHash is required")

        // Uppercase hex is refused — the schema is lowercase-only.
        let upper = msgHashPlaintext("hi").uppercased()
        let uppercase = try await client.request(
            "POST", "/api/rooms/\(room.id)/messages",
            body: Data(#"{"content":"hi","msgHash":"\#(upper)"}"#.utf8))
        XCTAssertEqual(uppercase.status, 400, "uppercase msgHash must be refused")

        // Wrong length is refused.
        let short = try await client.request(
            "POST", "/api/rooms/\(room.id)/messages",
            body: Data(#"{"content":"hi","msgHash":"abc123"}"#.utf8))
        XCTAssertEqual(short.status, 400)

        // And the correct hash still works after all that.
        _ = try await client.sendMessage(roomId: room.id, content: "hi")
    }

    func testOversizedBodyIs413() async throws {
        let (client, _, _) = try await server.loginFreshUser()
        let room = try await client.createRoom(name: "big")
        // > 100KB total body.
        let huge = String(repeating: "a", count: 110_000)
        let response = try await client.request(
            "POST", "/api/rooms/\(room.id)/messages",
            body: Data(#"{"content":"\#(huge)","msgHash":"\#(msgHashPlaintext("x"))"}"#.utf8))
        XCTAssertEqual(response.status, 413)
    }

    // MARK: Concurrency

    func testParallelSendsGetDistinctSerials() async throws {
        let (client, _, _) = try await server.loginFreshUser()
        let room = try await client.createRoom(name: "parallel")
        let count = 8

        let sent: [Message] = try await withThrowingTaskGroup(of: Message.self) { group in
            for i in 0..<count {
                group.addTask {
                    try await client.sendMessage(roomId: room.id, content: "parallel \(i)")
                }
            }
            var results: [Message] = []
            for try await message in group {
                results.append(message)
            }
            return results
        }

        XCTAssertEqual(sent.count, count)
        let serials = Set(sent.map(\.msgSerial))
        XCTAssertEqual(serials.count, count, "every message must get its own msgSerial")

        let listed = try await client.messages(roomId: room.id, limit: 100)
        let contents = Set(listed.map(\.content))
        for i in 0..<count {
            XCTAssertTrue(contents.contains("parallel \(i)"), "message \(i) must be listed")
        }
    }
}
