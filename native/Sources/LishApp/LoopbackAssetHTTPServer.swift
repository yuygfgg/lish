import Foundation
import Network

/// Errors reported by the authenticated static asset listener.
public enum AssetHTTPServerError: Error, CustomStringConvertible, Sendable {
    case invalidConfiguration(String)
    case listenerFailed(String)
    case stopped

    public var description: String {
        switch self {
        case .invalidConfiguration(let message): return message
        case .listenerFailed(let message): return "asset HTTP listener failed: \(message)"
        case .stopped: return "asset HTTP server is stopped"
        }
    }
}

/// Configuration for one loopback listener that serves the product page.
public struct AssetHTTPServerConfiguration: Sendable {
    public let rootURL: URL
    public let capability: String
    public let port: UInt16

    public init(rootURL: URL, capability: String, port: UInt16 = 0) throws {
        let root = rootURL.standardizedFileURL
        var isDirectory: ObjCBool = false
        guard FileManager.default.fileExists(atPath: root.path, isDirectory: &isDirectory),
              isDirectory.boolValue else {
            throw AssetHTTPServerError.invalidConfiguration("asset root is not a directory")
        }
        guard Self.isToken(capability, minimumLength: 32, maximumLength: 128) else {
            throw AssetHTTPServerError.invalidConfiguration(
                "capability must contain 32 to 128 URL-safe ASCII characters"
            )
        }
        self.rootURL = root
        self.capability = capability
        self.port = port
    }

    public var routePrefix: String {
        "/s/\(capability)/assets"
    }

    public func pageURL(port: UInt16, path: String = "index.html") -> URL {
        URL(string: "http://127.0.0.1:\(port)\(routePrefix)/\(path)")!
    }

    private static func isToken(_ value: String, minimumLength: Int, maximumLength: Int) -> Bool {
        let bytes = Array(value.utf8)
        guard bytes.count >= minimumLength, bytes.count <= maximumLength else { return false }
        return bytes.allSatisfy { byte in
            (byte >= 48 && byte <= 57) ||
                (byte >= 65 && byte <= 90) ||
                (byte >= 97 && byte <= 122) ||
                byte == 45 || byte == 95
        }
    }
}

/// Small, bounded HTTP/1.1 server for bundled, immutable application assets.
///
/// The listener handles one request per connection. The implementation shares
/// the same loopback and capability boundary as the disk and network services,
/// while keeping the asset parser independent from mutable disk I/O.
public final class LoopbackAssetHTTPServer: @unchecked Sendable {
    private final class State: @unchecked Sendable {
        let lock = NSLock()
        var listener: NWListener?
        var lifecycle: Lifecycle = .idle
        var startCompletion: ((Result<UInt16, Error>) -> Void)?
        var sessions: [ObjectIdentifier: AssetHTTPSession] = [:]
    }

    private enum Lifecycle {
        case idle
        case starting
        case running
        case stopped
    }

    private let configuration: AssetHTTPServerConfiguration
    private let queue = DispatchQueue(label: "io.lish.asset-http")
    private let state = State()

    public init(configuration: AssetHTTPServerConfiguration) {
        self.configuration = configuration
    }

    public func pageURL(port: UInt16, path: String = "index.html") -> URL {
        configuration.pageURL(port: port, path: path)
    }

    deinit {
        stop()
    }

    public func start(completion: @escaping @Sendable (Result<UInt16, Error>) -> Void) {
        state.lock.lock()
        guard state.lifecycle == .idle else {
            state.lock.unlock()
            completion(.failure(AssetHTTPServerError.invalidConfiguration("server already started or stopped")))
            return
        }
        state.lifecycle = .starting
        state.startCompletion = completion
        state.lock.unlock()

        do {
            let parameters = NWParameters(tls: nil, tcp: NWProtocolTCP.Options())
            parameters.acceptLocalOnly = true
            parameters.allowLocalEndpointReuse = false
            let requestedPort = configuration.port == 0
                ? NWEndpoint.Port.any
                : NWEndpoint.Port(rawValue: configuration.port)!
            let listener = try NWListener(using: parameters, on: requestedPort)
            listener.stateUpdateHandler = { [weak self] listenerState in
                self?.handleListenerState(listenerState)
            }
            listener.newConnectionHandler = { [weak self] connection in
                self?.accept(connection)
            }
            state.lock.lock()
            guard state.lifecycle == .starting else {
                state.lock.unlock()
                listener.cancel()
                return
            }
            state.listener = listener
            state.lock.unlock()
            listener.start(queue: queue)
        } catch {
            finishStart(.failure(error))
        }
    }

    public func stop() {
        state.lock.lock()
        state.lifecycle = .stopped
        let listener = state.listener
        state.listener = nil
        let sessions = Array(state.sessions.values)
        state.sessions.removeAll()
        let completion = state.startCompletion
        state.startCompletion = nil
        state.lock.unlock()

        listener?.cancel()
        sessions.forEach { $0.stop() }
        completion?(.failure(AssetHTTPServerError.stopped))
    }

    private func handleListenerState(_ listenerState: NWListener.State) {
        switch listenerState {
        case .ready:
            state.lock.lock()
            let port = state.listener?.port?.rawValue
            state.lock.unlock()
            if let port {
                finishStart(.success(port))
            } else {
                finishStart(.failure(AssetHTTPServerError.listenerFailed("listener has no port")))
            }
        case .failed(let error):
            finishStart(.failure(AssetHTTPServerError.listenerFailed(String(describing: error))))
            stop()
        default:
            break
        }
    }

    private func accept(_ connection: NWConnection) {
        state.lock.lock()
        guard state.listener != nil, state.sessions.count < 8 else {
            state.lock.unlock()
            connection.cancel()
            return
        }
        let session = AssetHTTPSession(
            connection: connection,
            configuration: configuration,
            queue: queue,
            onStop: { [weak self] session in self?.remove(session) }
        )
        state.sessions[ObjectIdentifier(session)] = session
        state.lock.unlock()
        session.start()
    }

    private func remove(_ session: AssetHTTPSession) {
        state.lock.lock()
        state.sessions.removeValue(forKey: ObjectIdentifier(session))
        state.lock.unlock()
    }

    private func finishStart(_ result: Result<UInt16, Error>) {
        state.lock.lock()
        guard state.lifecycle == .starting else {
            state.lock.unlock()
            return
        }
        state.lifecycle = result.isSuccess ? .running : .stopped
        let completion = state.startCompletion
        state.startCompletion = nil
        state.lock.unlock()
        completion?(result)
    }
}

private final class AssetHTTPSession: @unchecked Sendable {
    private static let maximumHeaderBytes = 16 * 1024

    private let connection: NWConnection
    private let configuration: AssetHTTPServerConfiguration
    private let queue: DispatchQueue
    private let onStop: @Sendable (AssetHTTPSession) -> Void
    private var buffer = Data()
    private var stopped = false

    init(
        connection: NWConnection,
        configuration: AssetHTTPServerConfiguration,
        queue: DispatchQueue,
        onStop: @escaping @Sendable (AssetHTTPSession) -> Void
    ) {
        self.connection = connection
        self.configuration = configuration
        self.queue = queue
        self.onStop = onStop
    }

    func start() {
        connection.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready: receive()
            case .failed, .cancelled: stop()
            default: break
            }
        }
        connection.start(queue: queue)
    }

    func stop() {
        guard !stopped else { return }
        stopped = true
        connection.cancel()
        onStop(self)
    }

    private func receive() {
        guard !stopped else { return }
        connection.receive(minimumIncompleteLength: 1, maximumLength: Self.maximumHeaderBytes) {
            [weak self] data, _, isComplete, error in
            guard let self else { return }
            if error != nil {
                stop()
                return
            }
            if let data { buffer.append(data) }
            guard buffer.count <= Self.maximumHeaderBytes else {
                respond(status: 431, body: Data("request headers too large".utf8))
                return
            }
            guard let end = buffer.range(of: Data([13, 10, 13, 10])) else {
                if isComplete {
                    respond(status: 400, body: Data("incomplete request headers".utf8))
                } else {
                    receive()
                }
                return
            }
            do {
                try perform(parseRequest(end: end))
            } catch let error as AssetRequestError {
                respond(status: error.status, body: Data(error.message.utf8))
            } catch {
                respond(status: 400, body: Data("invalid HTTP request".utf8))
            }
        }
    }

    private func parseRequest(end: Range<Data.Index>) throws -> AssetRequest {
        let headerData = buffer.subdata(in: buffer.startIndex..<end.lowerBound)
        guard let text = String(data: headerData, encoding: .utf8) else {
            throw AssetRequestError(status: 400, message: "invalid headers")
        }
        var lines = text.components(separatedBy: "\r\n")
        guard let requestLine = lines.first else {
            throw AssetRequestError(status: 400, message: "missing request line")
        }
        lines.removeFirst()
        let parts = requestLine.split(separator: " ", omittingEmptySubsequences: true)
        guard parts.count == 3, parts[2] == "HTTP/1.1" else {
            throw AssetRequestError(status: 400, message: "HTTP/1.1 is required")
        }
        var headers: [String: String] = [:]
        for line in lines where !line.isEmpty {
            guard let separator = line.firstIndex(of: ":") else {
                throw AssetRequestError(status: 400, message: "invalid header")
            }
            let name = line[..<separator].lowercased()
            let value = line[line.index(after: separator)...].trimmingCharacters(in: .whitespaces)
            guard headers[name] == nil else {
                throw AssetRequestError(status: 400, message: "duplicate header")
            }
            headers[name] = value
        }
        guard headers["transfer-encoding"] == nil else {
            throw AssetRequestError(status: 400, message: "chunked encoding is not supported")
        }
        guard (headers["content-length"].flatMap(Int.init) ?? 0) == 0 else {
            throw AssetRequestError(status: 400, message: "request body is not supported")
        }
        guard let target = URLComponents(string: String(parts[1])),
              target.query == nil,
              target.fragment == nil else {
            throw AssetRequestError(status: 400, message: "invalid request target")
        }
        return AssetRequest(method: String(parts[0]), path: target.percentEncodedPath)
    }

    private func perform(_ request: AssetRequest) throws {
        guard request.method == "GET" || request.method == "HEAD" else {
            respond(status: 405, body: Data("method not allowed".utf8))
            return
        }
        let prefix = configuration.routePrefix + "/"
        guard request.path.hasPrefix(prefix) else {
            respond(status: 404, body: Data("route not found".utf8))
            return
        }
        let encodedRelative = String(request.path.dropFirst(prefix.count))
        guard let relative = encodedRelative.removingPercentEncoding,
              !relative.isEmpty,
              !relative.contains("\\"),
              !relative.split(separator: "/").contains("..") else {
            respond(status: 400, body: Data("invalid asset path".utf8))
            return
        }
        let root = configuration.rootURL
        var fileURL = root.appendingPathComponent(relative).standardizedFileURL
        guard fileURL.path == root.path || fileURL.path.hasPrefix(root.path + "/") else {
            respond(status: 403, body: Data("asset path escaped root".utf8))
            return
        }
        var isDirectory: ObjCBool = false
        if FileManager.default.fileExists(atPath: fileURL.path, isDirectory: &isDirectory),
           isDirectory.boolValue {
            fileURL.appendPathComponent("index.html")
        }
        guard FileManager.default.fileExists(atPath: fileURL.path),
              let body = try? Data(contentsOf: fileURL) else {
            respond(status: 404, body: Data("asset not found".utf8))
            return
        }
        respond(
            status: 200,
            body: request.method == "HEAD" ? Data() : body,
            contentLength: body.count,
            contentType: mimeType(for: fileURL.pathExtension)
        )
    }

    private func respond(
        status: Int,
        body: Data,
        contentLength: Int? = nil,
        contentType: String = "text/plain; charset=utf-8"
    ) {
        guard !stopped else { return }
        let reason = status == 200 ? "OK" : status == 204 ? "No Content" : status == 404 ? "Not Found" : status == 405 ? "Method Not Allowed" : status == 431 ? "Request Header Fields Too Large" : "Bad Request"
        var response = Data("HTTP/1.1 \(status) \(reason)\r\nConnection: close\r\n".utf8)
        response.append(Data("Content-Type: \(contentType)\r\nContent-Length: \(contentLength ?? body.count)\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self' http://127.0.0.1:* http://[::1]:* ws://127.0.0.1:* ws://[::1]:*; font-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'\r\n\r\n".utf8))
        response.append(body)
        connection.send(
            content: response,
            contentContext: .finalMessage,
            isComplete: true,
            completion: .contentProcessed { [weak self] _ in self?.stop() }
        )
    }

    private func mimeType(for fileExtension: String) -> String {
        switch fileExtension.lowercased() {
        case "html": return "text/html; charset=utf-8"
        case "js", "mjs": return "text/javascript; charset=utf-8"
        case "css": return "text/css; charset=utf-8"
        case "wasm": return "application/wasm"
        case "json": return "application/json"
        case "svg": return "image/svg+xml"
        case "png": return "image/png"
        case "jpg", "jpeg": return "image/jpeg"
        case "woff", "woff2": return "font/woff2"
        default: return "application/octet-stream"
        }
    }
}

private struct AssetRequest {
    let method: String
    let path: String
}

private struct AssetRequestError: Error {
    let status: Int
    let message: String
}

private extension Result where Failure == Error {
    var isSuccess: Bool {
        if case .success = self { return true }
        return false
    }
}
