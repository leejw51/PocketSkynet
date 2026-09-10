// The pskynet-swift executable, driven as a subprocess: happy paths, exit
// codes, and the round trip create-room → send → messages.

import Foundation
import PocketSkynetClient
import XCTest

final class CLIIntegrationTests: XCTestCase {
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

    /// The build products directory this test bundle was built into —
    /// `swift test` builds the executable right next to it.
    static var productsDirectory: URL {
        for bundle in Bundle.allBundles where bundle.bundlePath.hasSuffix(".xctest") {
            return bundle.bundleURL.deletingLastPathComponent()
        }
        fatalError("couldn't find the products directory")
    }

    struct CLIResult {
        let exitCode: Int32
        let stdout: String
        let stderr: String
    }

    /// Run `pskynet-swift` with `arguments` against the suite's server.
    @discardableResult
    func runCLI(_ arguments: [String], key: String? = nil, envKey: String? = nil) throws -> CLIResult {
        let process = Process()
        process.executableURL = Self.productsDirectory.appendingPathComponent("pskynet-swift")
        var full = arguments
        full.append(contentsOf: ["--server", server.baseURL])
        if let key {
            full.append(contentsOf: ["--key", key])
        }
        process.arguments = full

        var environment = ProcessInfo.processInfo.environment
        environment.removeValue(forKey: "POCKETSKYNET_KEY")
        if let envKey {
            environment["POCKETSKYNET_KEY"] = envKey
        }
        process.environment = environment

        let out = Pipe()
        let err = Pipe()
        process.standardOutput = out
        process.standardError = err
        try process.run()
        // Read before waiting, or a chatty child can fill the pipe and stall.
        let stdout = out.fileHandleForReading.readDataToEndOfFile()
        let stderr = err.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return CLIResult(
            exitCode: process.terminationStatus,
            stdout: String(data: stdout, encoding: .utf8) ?? "",
            stderr: String(data: stderr, encoding: .utf8) ?? "")
    }

    private func freshKeyHex() throws -> (key: String, address: String) {
        // Random key via a random wallet, then re-import to get the address.
        var bytes = [UInt8](repeating: 0, count: 32)
        repeat {
            for i in 0..<32 { bytes[i] = UInt8.random(in: 0...255) }
        } while (try? EthereumWallet(privateKeyBytes: bytes)) == nil
        let hex = "0x" + Hex.encode(bytes)
        return (hex, try EthereumWallet(privateKeyHex: hex).address)
    }

    func testHealthExitsZero() throws {
        let result = try runCLI(["health"])
        XCTAssertEqual(result.exitCode, 0, result.stderr)
        XCTAssertTrue(result.stdout.contains("status: ok"), result.stdout)
        XCTAssertTrue(result.stdout.contains("transport: http/1.1"), result.stdout)
    }

    func testHealthAgainstADeadPortExitsOne() throws {
        let deadPort = TestServer.freeTCPPort() // allocated and released — nobody listens
        let process = Process()
        process.executableURL = Self.productsDirectory.appendingPathComponent("pskynet-swift")
        process.arguments = ["health", "--server", "http://127.0.0.1:\(deadPort)"]
        process.standardOutput = Pipe()
        process.standardError = Pipe()
        try process.run()
        process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 1)
    }

    func testLoginPrintsIdentityAndToken() throws {
        let (key, address) = try freshKeyHex()
        let result = try runCLI(["login", "--username", "swift_cli_user"], key: key)
        XCTAssertEqual(result.exitCode, 0, result.stderr)
        XCTAssertTrue(result.stdout.contains(address), result.stdout)
        XCTAssertTrue(result.stdout.contains("swift_cli_user"), result.stdout)
        XCTAssertTrue(result.stdout.contains("token:"), result.stdout)
    }

    func testKeyComesFromTheEnvironmentToo() throws {
        let (key, address) = try freshKeyHex()
        let result = try runCLI(["login"], envKey: key)
        XCTAssertEqual(result.exitCode, 0, result.stderr)
        XCTAssertTrue(result.stdout.contains(address), result.stdout)
    }

    func testMissingKeyIsAUsageErrorExit64() throws {
        let result = try runCLI(["rooms"])
        XCTAssertEqual(result.exitCode, 64, result.stderr)
        XCTAssertTrue(result.stderr.contains("POCKETSKYNET_KEY"), result.stderr)
    }

    func testMalformedKeyExitsOne() throws {
        let result = try runCLI(["login"], key: "0xnothex")
        XCTAssertEqual(result.exitCode, 1, result.stderr)
        XCTAssertTrue(result.stderr.contains("error"), result.stderr)
    }

    func testRoomsListsTheBuiltins() throws {
        let (key, address) = try freshKeyHex()
        let result = try runCLI(["rooms"], key: key)
        XCTAssertEqual(result.exitCode, 0, result.stderr)
        XCTAssertTrue(result.stdout.contains("room_note_\(address)"), result.stdout)
        XCTAssertTrue(result.stdout.contains("My Jarvis"), result.stdout)
        XCTAssertTrue(result.stdout.contains("My Lobby"), result.stdout)
    }

    func testCreateSendListRoundTrip() throws {
        let (key, _) = try freshKeyHex()
        let created = try runCLI(["create-room", "cli room"], key: key)
        XCTAssertEqual(created.exitCode, 0, created.stderr)
        let roomId = created.stdout.trimmingCharacters(in: .whitespacesAndNewlines)
        XCTAssertTrue(roomId.hasPrefix("room_"), created.stdout)

        let sent = try runCLI(["send", roomId, "hello from the CLI"], key: key)
        XCTAssertEqual(sent.exitCode, 0, sent.stderr)
        XCTAssertTrue(sent.stdout.contains("serial="), sent.stdout)

        let listed = try runCLI(["messages", roomId, "--limit", "10"], key: key)
        XCTAssertEqual(listed.exitCode, 0, listed.stderr)
        XCTAssertTrue(listed.stdout.contains("hello from the CLI"), listed.stdout)
    }

    func testSendToForeignRoomExitsOne() throws {
        let (aliceKey, _) = try freshKeyHex()
        let (bobKey, _) = try freshKeyHex()
        let created = try runCLI(["create-room", "alice cli room"], key: aliceKey)
        XCTAssertEqual(created.exitCode, 0, created.stderr)
        let roomId = created.stdout.trimmingCharacters(in: .whitespacesAndNewlines)

        let denied = try runCLI(["send", roomId, "intrusion"], key: bobKey)
        XCTAssertEqual(denied.exitCode, 1, denied.stderr)
        XCTAssertTrue(denied.stderr.contains("Access denied"), denied.stderr)
    }

    func testUnknownSubcommandFailsWithUsage() throws {
        let result = try runCLI(["frobnicate"])
        XCTAssertNotEqual(result.exitCode, 0)
    }
}
