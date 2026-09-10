// Wire coding: camelCase request bodies (with optionals omitted, not null),
// tolerant response decoding (nulls, omitted fields, unknown fields), and the
// three §1.5 error-envelope shapes.

import XCTest
@testable import PocketSkynetClient

final class WireCodingTests: XCTestCase {
    private func keys(of data: Data) throws -> Set<String> {
        guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            XCTFail("body is not a JSON object")
            return []
        }
        return Set(object.keys)
    }

    // MARK: Requests

    func testLoginRequestOmitsUsernameWhenNil() throws {
        let request = LoginRequest(walletAddress: "0xabc", challengeId: "id", signature: "0xsig")
        let data = try JSONEncoder().encode(request)
        XCTAssertEqual(try keys(of: data), ["walletAddress", "challengeId", "signature"],
                       "username must be omitted entirely, never null")
    }

    func testLoginRequestCarriesUsernameWhenPresent() throws {
        let request = LoginRequest(
            walletAddress: "0xabc", challengeId: "id", signature: "0xsig", username: "alice")
        let data = try JSONEncoder().encode(request)
        XCTAssertEqual(try keys(of: data), ["walletAddress", "challengeId", "signature", "username"])
        let object = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        XCTAssertEqual(object?["username"] as? String, "alice")
        XCTAssertEqual(object?["walletAddress"] as? String, "0xabc")
    }

    func testChallengeRequestUsesCamelCase() throws {
        let data = try JSONEncoder().encode(ChallengeRequest(walletAddress: "0xdef"))
        XCTAssertEqual(try keys(of: data), ["walletAddress"])
    }

    func testCreateRoomRequestOmitsNilDescription() throws {
        XCTAssertEqual(try keys(of: JSONEncoder().encode(CreateRoomRequest(name: "Team"))),
                       ["name"])
        XCTAssertEqual(
            try keys(of: JSONEncoder().encode(CreateRoomRequest(name: "Team", description: "d"))),
            ["name", "description"])
    }

    func testSendMessageRequestShape() throws {
        let request = SendMessageRequest(content: "hi", msgHash: msgHashPlaintext("hi"))
        XCTAssertEqual(try keys(of: JSONEncoder().encode(request)), ["content", "msgHash"])
    }

    // MARK: Responses

    func testMessageDecodesWithNullsAndUnknownFields() throws {
        let json = """
        {
          "id": "msg_1749652746620_4cfe1c4c",
          "roomId": "room_x",
          "senderAddress": "0x742d35cc",
          "content": "Hello everyone!",
          "msgHash": "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
          "messageTimestamp": 1749652746620,
          "msgType": "add",
          "msgSerial": 1749652746620,
          "isDeleted": false,
          "editedAt": null,
          "createdAt": "2025-06-11T14:39:06.000Z",
          "isEncrypted": false,
          "iv": null,
          "hmac": null,
          "encVer": 1,
          "keyVersion": 1,
          "txHash": null,
          "targetMessageId": null,
          "emoticonCode": null,
          "someFutureField": {"nested": true},
          "sender": {
            "walletAddress": "0x742d35cc",
            "username": "alice",
            "publicKey": null,
            "publicKeySig": null,
            "createdAt": "2025-06-11T14:39:06.000Z",
            "updatedAt": "2025-06-11T14:39:06.000Z"
          }
        }
        """
        let message = try JSONDecoder().decode(Message.self, from: Data(json.utf8))
        XCTAssertEqual(message.id, "msg_1749652746620_4cfe1c4c")
        XCTAssertEqual(message.messageTimestamp, 1_749_652_746_620)
        XCTAssertEqual(message.msgSerial, 1_749_652_746_620)
        XCTAssertNil(message.editedAt)
        XCTAssertNil(message.iv)
        XCTAssertEqual(message.sender?.username, "alice")
        XCTAssertNil(message.sender?.publicKey)
        XCTAssertNil(message.replyCount, "absent means absent, not 0")
    }

    func testMessageDecodesWithOmittedOptionalFields() throws {
        // A minimal message — everything nullable simply missing.
        let json = """
        {
          "id": "msg_1_a", "roomId": "room_x", "senderAddress": "0xabc",
          "content": "hi", "messageTimestamp": 5, "msgType": "add",
          "msgSerial": 5, "isDeleted": false
        }
        """
        let message = try JSONDecoder().decode(Message.self, from: Data(json.utf8))
        XCTAssertNil(message.sender)
        XCTAssertNil(message.msgHash)
        XCTAssertNil(message.txHash)
    }

    func testRoomDecodesBareAndEnriched() throws {
        let bare = """
        {"id":"room_1_a","name":"Team","description":null,
         "currentKeyVersion":1,"keyRotationPending":false,
         "createdAt":"2025-06-11T14:38:59.000Z"}
        """
        let room = try JSONDecoder().decode(Room.self, from: Data(bare.utf8))
        XCTAssertNil(room.memberCount)
        XCTAssertNil(room.unreadCount)

        let enriched = """
        {"id":"room_1_a","name":"Team","description":"d","kind":"channel",
         "currentKeyVersion":1,"keyRotationPending":false,
         "createdAt":"2025-06-11T14:38:59.000Z",
         "memberCount":3,"members":[],"admins":[],"hasEncryption":false,
         "unreadCount":4,"lastReadSerial":1749652746620}
        """
        let full = try JSONDecoder().decode(Room.self, from: Data(enriched.utf8))
        XCTAssertEqual(full.memberCount, 3)
        XCTAssertEqual(full.unreadCount, 4)
        XCTAssertEqual(full.lastReadSerial, 1_749_652_746_620)
        XCTAssertEqual(full.kind, "channel")
    }

    func testServerInfoMapsProtocolKeyword() throws {
        let json = """
        {"protocol":"h3","scheme":"http","port":9099,"http3Port":9101,
         "http3Available":true,"uptime":12,"unknown":[1,2,3]}
        """
        let info = try JSONDecoder().decode(ServerInfo.self, from: Data(json.utf8))
        XCTAssertEqual(info.protocolName, "h3")
        XCTAssertEqual(info.http3Port, 9101)
    }

    // MARK: Error envelopes — all three §1.5 shapes

    func testPlainMessageEnvelope() throws {
        let envelope = try JSONDecoder().decode(
            ErrorEnvelope.self, from: Data(#"{"message":"Access denied"}"#.utf8))
        XCTAssertEqual(envelope.message, "Access denied")
        XCTAssertNil(envelope.errors)
        XCTAssertNil(envelope.code)
    }

    func testValidationFailedEnvelope() throws {
        let json = #"{"message":"Validation failed","errors":["roomId: Room ID contains invalid characters"]}"#
        let envelope = try JSONDecoder().decode(ErrorEnvelope.self, from: Data(json.utf8))
        XCTAssertEqual(envelope.message, "Validation failed")
        XCTAssertEqual(envelope.errors, ["roomId: Room ID contains invalid characters"])
    }

    func testMachineCodeEnvelope() throws {
        let json = #"{"code":"KEY_ROTATION_REQUIRED","message":"rotate first","currentKeyVersion":3}"#
        let envelope = try JSONDecoder().decode(ErrorEnvelope.self, from: Data(json.utf8))
        XCTAssertEqual(envelope.code, "KEY_ROTATION_REQUIRED")
        XCTAssertEqual(envelope.currentKeyVersion, 3)
        XCTAssertEqual(envelope.message, "rotate first")
    }

    func testAPIErrorDescriptionCarriesTheEnvelope() {
        let error = APIError.http(
            status: 400,
            envelope: ErrorEnvelope(message: "Validation failed", errors: ["msgHash: bad"]),
            body: "{}")
        XCTAssertTrue("\(error)".contains("Validation failed"))
        XCTAssertTrue("\(error)".contains("msgHash: bad"))
        XCTAssertEqual(error.status, 400)
    }
}
