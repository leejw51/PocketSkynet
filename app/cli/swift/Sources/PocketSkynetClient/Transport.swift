// One transport abstraction, two wire protocols.
//
// `URLSessionTransport` carries both: HTTP/1.1(+TLS) as URLSession's default,
// and HTTP/3 by setting `assumesHTTP3Capable` on each request. On macOS,
// URLSession is backed by Network.framework, which negotiates QUIC natively —
// verified against this server: `GET /api/server/info` on the QUIC port
// reports `"protocol":"h3"`. No external curl is involved.

import Foundation

public struct TransportResponse {
    public let status: Int
    public let headers: [String: String]
    public let body: Data

    public init(status: Int, headers: [String: String], body: Data) {
        self.status = status
        self.headers = headers
        self.body = body
    }
}

public protocol Transport {
    func send(method: String, url: URL, headers: [String: String], body: Data?) async throws -> TransportResponse
}

public enum TransportError: Error, CustomStringConvertible {
    case notHTTP
    case insecureServerCertificate(Error)

    public var description: String {
        switch self {
        case .notHTTP:
            return "response was not HTTP"
        case .insecureServerCertificate(let underlying):
            return "TLS trust failed (self-signed dev server? pass --insecure): \(underlying)"
        }
    }
}

public final class URLSessionTransport: NSObject, Transport {
    private let insecure: Bool
    private let http3: Bool
    private var session: URLSession!

    /// - Parameters:
    ///   - insecure: accept any server certificate (self-signed dev servers).
    ///   - http3: mark every request `assumesHTTP3Capable`, so URLSession
    ///     attempts QUIC on the URL's port. Requires an `https` URL.
    public init(insecure: Bool = false, http3: Bool = false, timeout: TimeInterval = 30) {
        self.insecure = insecure
        self.http3 = http3
        super.init()
        let config = URLSessionConfiguration.ephemeral
        config.timeoutIntervalForRequest = timeout
        // A CLI must see the server's answer, not a stale intermediary's.
        config.requestCachePolicy = .reloadIgnoringLocalCacheData
        self.session = URLSession(configuration: config, delegate: self, delegateQueue: nil)
    }

    public func send(method: String, url: URL, headers: [String: String], body: Data?) async throws -> TransportResponse {
        var request = URLRequest(url: url)
        request.httpMethod = method
        if http3 {
            request.assumesHTTP3Capable = true
        }
        for (name, value) in headers {
            request.setValue(value, forHTTPHeaderField: name)
        }
        request.httpBody = body

        let (data, response): (Data, URLResponse)
        do {
            (data, response) = try await session.data(for: request)
        } catch let error as NSError
            where error.domain == NSURLErrorDomain && error.code == NSURLErrorServerCertificateUntrusted {
            throw TransportError.insecureServerCertificate(error)
        }
        guard let http = response as? HTTPURLResponse else {
            throw TransportError.notHTTP
        }
        var headerMap: [String: String] = [:]
        for (name, value) in http.allHeaderFields {
            if let n = name as? String, let v = value as? String {
                headerMap[n.lowercased()] = v
            }
        }
        return TransportResponse(status: http.statusCode, headers: headerMap, body: data)
    }
}

extension URLSessionTransport: URLSessionDelegate {
    public func urlSession(
        _ session: URLSession,
        didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        guard insecure,
              challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
              let trust = challenge.protectionSpace.serverTrust
        else {
            // Without --insecure the platform's normal trust evaluation runs,
            // and a self-signed certificate is refused — deliberately.
            completionHandler(.performDefaultHandling, nil)
            return
        }
        completionHandler(.useCredential, URLCredential(trust: trust))
    }
}
