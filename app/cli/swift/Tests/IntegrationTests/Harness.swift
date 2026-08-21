// Boots a real `pocketskynet` process per suite, modeled on
// `app/server/tests/common/harness.rs`:
//
// * ephemeral ports (TCP and UDP live in different namespaces, so each is
//   probed in its own),
// * boots serialized under a lock — "ask the OS for a free port, close it,
//   hand it to a child" has a race window, and the loser of a bind race gets
//   its `/api/health` probe answered by the *winner*, so after health goes
//   green we re-check that our own child is still alive,
// * a temp data directory per server, removed on teardown,
// * teardown in class tearDown (runs even when tests fail) plus deinit.
//
// The server binary is found by walking up from this file for
// `app/target/{release,debug}/pocketskynet` — robust to worktrees and
// absorbed-submodule layouts, where the target dir lives above the checkout
// that holds the sources. `POCKETSKYNET_BIN` overrides. If no binary exists
// anywhere, one `cargo build --release` is attempted in the nearest `app/`.

import Foundation
import PocketSkynetClient
import XCTest

final class TestServer {
    struct BootFailure: Error, CustomStringConvertible {
        let message: String
        var description: String { message }
    }

    static let jwtSecret = "pocketskynet-integration-test-secret-0123456789abcdef"
    private static let bootLock = NSLock()
    private static let bootTimeout: TimeInterval = 30

    let process: Process
    let port: UInt16
    let http3Port: UInt16?
    let dataDir: URL
    let tls: Bool
    /// `http(s)://127.0.0.1:<port>` — the TCP listener.
    let baseURL: String

    /// The QUIC endpoint's base URL. HTTP/3 is always TLS.
    var http3URL: String {
        guard let http3Port else { preconditionFailure("this server has no HTTP/3 listener") }
        return "https://127.0.0.1:\(http3Port)"
    }

    var logPath: URL { dataDir.appendingPathComponent("server.log") }

    var serverLog: String {
        (try? String(contentsOf: logPath, encoding: .utf8)) ?? ""
    }

    private init(process: Process, port: UInt16, http3Port: UInt16?, dataDir: URL, tls: Bool) {
        self.process = process
        self.port = port
        self.http3Port = http3Port
        self.dataDir = dataDir
        self.tls = tls
        self.baseURL = "\(tls ? "https" : "http")://127.0.0.1:\(port)"
    }

    // MARK: Boot

    /// Start a server and block until `/api/health` answers 200 from *our*
    /// child. Retries the whole boot on a lost bind race.
    static func start(tls: Bool = false, http3: Bool = false) -> TestServer {
        var lastError = ""
        for _ in 0..<5 {
            switch tryStart(tls: tls, http3: http3) {
            case .success(let server): return server
            case .failure(let error): lastError = error.message
            }
        }
        fatalError("could not start pocketskynet after 5 attempts: \(lastError)")
    }

    private static func tryStart(tls: Bool, http3: Bool) -> Result<TestServer, BootFailure> {
        bootLock.lock()
        defer { bootLock.unlock() }

        let binary = locateBinary()
        let port = freeTCPPort()
        let quicPort = http3 ? freeUDPPort() : nil

        let dataDir = FileManager.default.temporaryDirectory
            .appendingPathComponent("ps-swift-it-\(ProcessInfo.processInfo.processIdentifier)-\(UUID().uuidString)")
        let staticDir = dataDir.appendingPathComponent("static")
        do {
            try FileManager.default.createDirectory(at: staticDir, withIntermediateDirectories: true)
        } catch {
            return .failure(BootFailure(message: "mkdir \(staticDir.path): \(error)"))
        }

        var arguments = [
            "--host", "127.0.0.1",
            "--port", String(port),
            "--data-dir", dataDir.path,
            "--static-dir", staticDir.path,
            "--jwt-secret", jwtSecret,
            "--no-rate-limit",
            "--no-payment-verify",
            "--no-mdns",
            "--log", "warn",
        ]
        if tls { arguments.append("--tls") }
        if let quicPort {
            arguments.append(contentsOf: ["--http3", "--http3-port", String(quicPort)])
        }

        let process = Process()
        process.executableURL = binary
        process.arguments = arguments
        process.environment = scrubbedEnvironment()
        FileManager.default.createFile(atPath: dataDir.appendingPathComponent("server.log").path, contents: nil)
        let log = FileHandle(forWritingAtPath: dataDir.appendingPathComponent("server.log").path)
        process.standardOutput = log
        process.standardError = log
        process.standardInput = FileHandle.nullDevice

        do {
            try process.run()
        } catch {
            return .failure(BootFailure(message: "spawn \(binary.path): \(error)"))
        }

        let server = TestServer(process: process, port: port, http3Port: quicPort, dataDir: dataDir, tls: tls)
        if let failure = server.awaitHealth() {
            let tail = server.serverLog.split(separator: "\n").suffix(30).joined(separator: "\n")
            server.stop()
            return .failure(BootFailure(message: "\(failure)\n--- server log (tail) ---\n\(tail)"))
        }
        return .success(server)
    }

    /// A developer's shell must not decide what the suite tests — drop every
    /// PS_*/VITE_* override, and neutralize values baked in at compile time.
    private static func scrubbedEnvironment() -> [String: String] {
        var env = ProcessInfo.processInfo.environment
        for key in Array(env.keys) where key.hasPrefix("PS_") || key.hasPrefix("VITE_") {
            env.removeValue(forKey: key)
        }
        env.removeValue(forKey: "POCKETSKYNET_PATH")
        env["PS_IGNORE_BAKED_ENV"] = "1"
        return env
    }

    /// `nil` when healthy; otherwise a description of what went wrong.
    private func awaitHealth() -> String? {
        let deadline = Date().addingTimeInterval(Self.bootTimeout)
        let url = "\(baseURL)/api/health"
        while Date() < deadline {
            if !process.isRunning {
                return "server exited during boot with status \(process.terminationStatus)"
            }
            if Self.syncGET(url, insecure: tls) == 200 {
                // Somebody answered — make sure it was us. If our child lost
                // the bind race it has already exited.
                if process.isRunning {
                    return nil
                }
                return "another process owns port \(port); our child exited with status \(process.terminationStatus)"
            }
            Thread.sleep(forTimeInterval: 0.02)
        }
        return "/api/health never became ready on port \(port)"
    }

    /// Blocking GET so the harness can run inside synchronous XCTest setUp.
    static func syncGET(_ url: String, insecure: Bool) -> Int? {
        let transport = URLSessionTransport(insecure: insecure, http3: false, timeout: 2)
        let semaphore = DispatchSemaphore(value: 0)
        var status: Int?
        Task.detached {
            defer { semaphore.signal() }
            guard let target = URL(string: url) else { return }
            status = try? await transport.send(method: "GET", url: target, headers: [:], body: nil).status
        }
        semaphore.wait()
        return status
    }

    // MARK: Teardown

    func stop() {
        if process.isRunning {
            kill(process.processIdentifier, SIGKILL)
            process.waitUntilExit()
        }
        try? FileManager.default.removeItem(at: dataDir)
    }

    deinit {
        if process.isRunning {
            kill(process.processIdentifier, SIGKILL)
        }
    }

    // MARK: Binary location

    private static var cachedBinary: URL?

    static func locateBinary() -> URL {
        if let cachedBinary { return cachedBinary }

        if let override = ProcessInfo.processInfo.environment["POCKETSKYNET_BIN"],
           FileManager.default.isExecutableFile(atPath: override) {
            let url = URL(fileURLWithPath: override)
            cachedBinary = url
            return url
        }

        if let found = searchAncestors() {
            cachedBinary = found
            return found
        }

        // No binary anywhere above us — build it once.
        if let appDir = nearestAppDir() {
            fputs("pocketskynet binary not found; running `cargo build --release` in \(appDir.path) (once)…\n", stderr)
            let build = Process()
            build.executableURL = URL(fileURLWithPath: "/usr/bin/env")
            build.arguments = ["cargo", "build", "--release", "--bin", "pocketskynet"]
            build.currentDirectoryURL = appDir
            do {
                try build.run()
                build.waitUntilExit()
            } catch {
                fatalError("could not run cargo: \(error)")
            }
            if let found = searchAncestors() {
                cachedBinary = found
                return found
            }
        }
        fatalError("""
            No pocketskynet server binary. Build it first:
                cd app && cargo build --release --bin pocketskynet
            or point POCKETSKYNET_BIN at one.
            """)
    }

    /// Walk up from this file, checking `app/target/{release,debug}/pocketskynet`
    /// under every ancestor. When both profiles exist, the newer build wins.
    private static func searchAncestors() -> URL? {
        var dir = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
        for _ in 0..<12 {
            let candidates = ["release", "debug"]
                .map { dir.appendingPathComponent("app/target/\($0)/pocketskynet") }
                .filter { FileManager.default.isExecutableFile(atPath: $0.path) }
            if !candidates.isEmpty {
                return candidates.max { modificationDate($0) < modificationDate($1) }
            }
            let parent = dir.deletingLastPathComponent()
            if parent.path == dir.path { break }
            dir = parent
        }
        return nil
    }

    private static func modificationDate(_ url: URL) -> Date {
        (try? FileManager.default.attributesOfItem(atPath: url.path)[.modificationDate] as? Date)
            .flatMap { $0 } ?? .distantPast
    }

    /// The nearest ancestor `app/` directory holding a Cargo.toml.
    private static func nearestAppDir() -> URL? {
        var dir = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
        for _ in 0..<12 {
            let candidate = dir.appendingPathComponent("app/Cargo.toml")
            if FileManager.default.fileExists(atPath: candidate.path) {
                return dir.appendingPathComponent("app")
            }
            let parent = dir.deletingLastPathComponent()
            if parent.path == dir.path { break }
            dir = parent
        }
        return nil
    }

    // MARK: Ports

    private static func freePort(socketType: Int32) -> UInt16 {
        let fd = socket(AF_INET, socketType, 0)
        precondition(fd >= 0, "socket() failed")
        defer { close(fd) }

        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = 0
        address.sin_addr = in_addr(s_addr: inet_addr("127.0.0.1"))

        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        precondition(bound == 0, "bind() failed: \(String(cString: strerror(errno)))")

        var assigned = sockaddr_in()
        var length = socklen_t(MemoryLayout<sockaddr_in>.size)
        let named = withUnsafeMutablePointer(to: &assigned) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                getsockname(fd, $0, &length)
            }
        }
        precondition(named == 0, "getsockname() failed")
        return UInt16(bigEndian: assigned.sin_port)
    }

    static func freeTCPPort() -> UInt16 { freePort(socketType: SOCK_STREAM) }

    /// TCP and UDP port numbers live in different namespaces; a QUIC listener
    /// needs a port probed as UDP.
    static func freeUDPPort() -> UInt16 { freePort(socketType: SOCK_DGRAM) }
}

// MARK: - Shared test conveniences

extension TestServer {
    /// A client for the TCP listener. TLS servers use self-signed certs, so
    /// their clients run with the trust override.
    func client() -> SkynetClient {
        SkynetClient(baseURL: URL(string: baseURL)!, insecure: tls, http3: false)
    }

    /// A client for the QUIC listener.
    func http3Client() -> SkynetClient {
        SkynetClient(baseURL: URL(string: http3URL)!, insecure: true, http3: true)
    }

    /// A fresh wallet logged in through the whole challenge flow.
    @discardableResult
    func loginFreshUser(username: String? = nil) async throws -> (SkynetClient, EthereumWallet, LoginResponse) {
        let wallet = try EthereumWallet.random()
        let client = client()
        let response = try await client.login(wallet: wallet, username: username)
        return (client, wallet, response)
    }
}
