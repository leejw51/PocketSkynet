// pskynet-swift — a small CLI over PocketSkynetClient.
//
// Exit codes: 0 success, 1 any runtime failure (API error, transport error,
// bad key), 64 argument-validation errors.

import ArgumentParser
import Foundation
import PocketSkynetClient

struct GlobalOptions: ParsableArguments {
    @Option(name: .long, help: "Server base URL (e.g. http://127.0.0.1:9099, or https://host:9101 with --http3).")
    var server: String = "http://127.0.0.1:9099"

    @Flag(name: .long, help: "Speak HTTP/3 (QUIC). The --server URL must be https and name the QUIC port.")
    var http3 = false

    @Flag(name: .long, help: "Accept self-signed server certificates (dev servers).")
    var insecure = false

    @Option(name: .long, help: "Wallet private key hex (or set POCKETSKYNET_KEY).")
    var key: String?

    @Option(name: .long, help: "Username for first-time login (otherwise one is generated).")
    var username: String?

    func makeClient() throws -> SkynetClient {
        guard let url = URL(string: server), url.scheme != nil else {
            throw ValidationError("--server is not a URL: \(server)")
        }
        if http3 && url.scheme != "https" {
            throw ValidationError("--http3 requires an https --server URL (QUIC mandates TLS).")
        }
        return SkynetClient(baseURL: url, insecure: insecure, http3: http3)
    }

    func wallet() throws -> EthereumWallet {
        let hex = key ?? ProcessInfo.processInfo.environment["POCKETSKYNET_KEY"]
        guard let hex, !hex.isEmpty else {
            throw ValidationError("no wallet key: pass --key or set POCKETSKYNET_KEY")
        }
        return try EthereumWallet(privateKeyHex: hex)
    }

    /// Run `body` with a client, closing its transport afterwards so the
    /// underlying URLSession is released (the session retains its delegate, so
    /// nothing frees it otherwise).
    func withClient<T>(_ body: (SkynetClient) async throws -> T) async throws -> T {
        let client = try makeClient()
        defer { client.close() }
        return try await body(client)
    }

    /// [`withClient`](withClient), after logging in first.
    func withAuthenticatedClient<T>(_ body: (SkynetClient) async throws -> T) async throws -> T {
        try await withClient { client in
            try await client.login(wallet: try wallet(), username: username)
            return try await body(client)
        }
    }
}

/// Shared error-to-exit-code mapping for every subcommand.
enum CLIRunner {
    static func run(_ body: () async throws -> Void) async {
        do {
            try await body()
        } catch let error as ValidationError {
            FileHandle.standardError.write(Data("error: \(error.message)\n".utf8))
            Foundation.exit(64)
        } catch {
            FileHandle.standardError.write(Data("error: \(error)\n".utf8))
            Foundation.exit(1)
        }
    }
}

@main
struct PSkynet: AsyncParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "pskynet-swift",
        abstract: "Swift client for the PocketSkynet messenger server.",
        subcommands: [Health.self, Login.self, Rooms.self, CreateRoom.self, Send.self, Messages.self]
    )
}

struct Health: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Probe /api/health.")
    @OptionGroup var options: GlobalOptions

    func run() async {
        await CLIRunner.run {
            try await options.withClient { client in
                let health = try await client.health()
                print("status: \(health.status)  uptime: \(health.uptime ?? 0)s")
                let info = try await client.serverInfo()
                print("transport: \(info.protocolName)")
            }
        }
    }
}

struct Login: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Challenge → sign → JWT.")
    @OptionGroup var options: GlobalOptions

    func run() async {
        await CLIRunner.run {
            try await options.withClient { client in
                let response = try await client.login(wallet: try options.wallet(), username: options.username)
                print("address:  \(response.user.walletAddress)")
                print("username: \(response.user.username)")
                print("token:    \(response.token)")
            }
        }
    }
}

struct Rooms: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "List the rooms you are a member of.")
    @OptionGroup var options: GlobalOptions

    func run() async {
        await CLIRunner.run {
            try await options.withAuthenticatedClient { client in
                for room in try await client.rooms() {
                    let kind = room.kind.map { " [\($0)]" } ?? ""
                    print("\(room.id)  \(room.name)\(kind)")
                }
            }
        }
    }
}

struct CreateRoom: AsyncParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "create-room", abstract: "Create a room and print its id.")
    @OptionGroup var options: GlobalOptions
    @Argument(help: "Room name.") var name: String

    func run() async {
        await CLIRunner.run {
            try await options.withAuthenticatedClient { client in
                let room = try await client.createRoom(name: name)
                print(room.id)
            }
        }
    }
}

struct Send: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "Send a plaintext message.")
    @OptionGroup var options: GlobalOptions
    @Argument(help: "Room id.") var roomId: String
    @Argument(help: "Message text.") var text: String

    func run() async {
        await CLIRunner.run {
            try await options.withAuthenticatedClient { client in
                let message = try await client.sendMessage(roomId: roomId, content: text)
                print("\(message.id)  serial=\(message.msgSerial)")
            }
        }
    }
}

struct Messages: AsyncParsableCommand {
    static let configuration = CommandConfiguration(abstract: "List messages in a room.")
    @OptionGroup var options: GlobalOptions
    @Argument(help: "Room id.") var roomId: String
    @Option(name: .long, help: "At most this many messages (1–100).") var limit: Int?

    func run() async {
        await CLIRunner.run {
            try await options.withAuthenticatedClient { client in
                for message in try await client.messages(roomId: roomId, limit: limit) {
                    let who = message.sender?.username ?? message.senderAddress
                    print("[\(message.msgSerial)] \(who): \(message.content)")
                }
            }
        }
    }
}
