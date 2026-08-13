import Foundation
import Network

public enum DiskHTTPServerError: Error, CustomStringConvertible, Sendable {
    case invalidConfiguration(String)
    case listenerFailed(String)
    case stopped

    public var description: String {
        switch self {
        case .invalidConfiguration(let message): return message
        case .listenerFailed(let message): return "disk HTTP listener failed: \(message)"
        case .stopped: return "disk HTTP server is stopped"
        }
    }
}

public struct DiskHTTPServerConfiguration: Sendable {
    public let capability: String
    public let vmID: String
    public let allowedOrigin: String
    public let port: UInt16
    public let maximumConnections: Int

    public init(
        capability: String,
        vmID: String,
        allowedOrigin: String,
        port: UInt16 = 0,
        maximumConnections: Int = 4
    ) throws {
        guard Self.isToken(capability, minimumLength: 32, maximumLength: 128) else {
            throw DiskHTTPServerError.invalidConfiguration(
                "capability must contain 32 to 128 URL-safe ASCII characters"
            )
        }
        guard Self.isToken(vmID, minimumLength: 1, maximumLength: 128) else {
            throw DiskHTTPServerError.invalidConfiguration(
                "vmID must contain 1 to 128 URL-safe ASCII characters"
            )
        }
        guard let origin = URL(string: allowedOrigin),
              let scheme = origin.scheme,
              scheme == "http" || scheme == "https",
              origin.host != nil,
              origin.user == nil,
              origin.password == nil,
              origin.path.isEmpty || origin.path == "/",
              origin.query == nil,
              origin.fragment == nil else {
            throw DiskHTTPServerError.invalidConfiguration("allowedOrigin must be an HTTP origin")
        }
        guard maximumConnections > 0 else {
            throw DiskHTTPServerError.invalidConfiguration("maximumConnections must be positive")
        }
        self.capability = capability
        self.vmID = vmID
        self.allowedOrigin = allowedOrigin.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        self.port = port
        self.maximumConnections = maximumConnections
    }

    public var diskPath: String {
        "/s/\(capability)/vms/\(vmID)/disk"
    }

    public var flushPath: String {
        "\(diskPath)/flush"
    }

    fileprivate func accepts(origin: String?) -> Bool {
        origin?.trimmingCharacters(in: .whitespacesAndNewlines) == allowedOrigin
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

/// Authenticated loopback HTTP service for one VM disk image.
///
/// The service closes every HTTP connection after one request. This keeps the
/// request parser small and bounds connection state while the Worker performs
/// one disk operation at a time.
public final class DiskHTTPServer: @unchecked Sendable {
    private final class State: @unchecked Sendable {
        let lock = NSLock()
        var listener: NWListener?
        var sessions: [ObjectIdentifier: DiskHTTPSession] = [:]
        var lifecycle: Lifecycle = .idle
        var startCompletion: ((Result<UInt16, Error>) -> Void)?
    }

    private enum Lifecycle {
        case idle
        case starting
        case running
        case stopped
    }

    private let store: DiskStore
    private let configuration: DiskHTTPServerConfiguration
    private let queue = DispatchQueue(label: "io.lish.disk-http")
    private let state = State()

    public init(store: DiskStore, configuration: DiskHTTPServerConfiguration) {
        self.store = store
        self.configuration = configuration
    }

    deinit {
        stop()
    }

    public func start(completion: @escaping @Sendable (Result<UInt16, Error>) -> Void) {
        state.lock.lock()
        guard state.lifecycle == .idle else {
            state.lock.unlock()
            completion(.failure(
                DiskHTTPServerError.invalidConfiguration("server already started or stopped")
            ))
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
        completion?(.failure(DiskHTTPServerError.stopped))
    }

    private func handleListenerState(_ listenerState: NWListener.State) {
        switch listenerState {
        case .ready:
            state.lock.lock()
            let port = state.listener?.port?.rawValue
            state.lock.unlock()
            if let port { finishStart(.success(port)) }
            else { finishStart(.failure(DiskHTTPServerError.listenerFailed("listener has no port"))) }
        case .failed(let error):
            finishStart(.failure(DiskHTTPServerError.listenerFailed(String(describing: error))))
            stop()
        default:
            break
        }
    }

    private func accept(_ connection: NWConnection) {
        state.lock.lock()
        guard state.listener != nil, state.sessions.count < configuration.maximumConnections else {
            state.lock.unlock()
            connection.cancel()
            return
        }
        let session = DiskHTTPSession(
            connection: connection,
            store: store,
            configuration: configuration,
            queue: queue,
            onStop: { [weak self] session in self?.remove(session) }
        )
        state.sessions[ObjectIdentifier(session)] = session
        state.lock.unlock()
        session.start()
    }

    private func remove(_ session: DiskHTTPSession) {
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

private final class DiskHTTPSession: @unchecked Sendable {
    private static let maximumHeaderBytes = 16 * 1024
    private static let maximumRequestBytes = DiskStore.defaultMaximumOperationBytes

    private let connection: NWConnection
    private let store: DiskStore
    private let configuration: DiskHTTPServerConfiguration
    private let queue: DispatchQueue
    private let onStop: @Sendable (DiskHTTPSession) -> Void
    private var buffer = Data()
    private var stopped = false

    init(
        connection: NWConnection,
        store: DiskStore,
        configuration: DiskHTTPServerConfiguration,
        queue: DispatchQueue,
        onStop: @escaping @Sendable (DiskHTTPSession) -> Void
    ) {
        self.connection = connection
        self.store = store
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
        connection.receive(minimumIncompleteLength: 1, maximumLength: Self.maximumHeaderBytes) { [weak self] data, _, isComplete, error in
            guard let self else { return }
            if error != nil {
                stop()
                return
            }
            guard let data else {
                if isComplete { stop() }
                else { receive() }
                return
            }
            buffer.append(data)
            guard buffer.count <= Self.maximumHeaderBytes else {
                respond(status: 431, body: Data("request headers too large".utf8))
                return
            }
            guard let headerEnd = buffer.range(of: Data([13, 10, 13, 10])) else {
                if isComplete {
                    respond(status: 400, body: Data("incomplete request headers".utf8))
                } else {
                    receive()
                }
                return
            }
            do {
                let request = try parseRequest(headerEnd: headerEnd)
                guard request.bodyLength <= Self.maximumRequestBytes else {
                    respond(status: 413, body: Data("request body too large".utf8))
                    return
                }
                let bodyStart = headerEnd.upperBound
                let receivedBodyLength = buffer.count - bodyStart
                guard receivedBodyLength <= request.bodyLength else {
                    respond(status: 400, body: Data("request body exceeds content length".utf8))
                    return
                }
                let missing = request.bodyLength - receivedBodyLength
                if missing > 0 {
                    if isComplete {
                        respond(status: 400, body: Data("incomplete request body".utf8))
                    } else {
                        receiveBody(request: request, bodyStart: bodyStart, missing: missing)
                    }
                } else {
                    perform(request: request, bodyStart: bodyStart)
                }
            } catch let error as RequestError {
                respond(status: error.status, body: Data(error.message.utf8))
            } catch {
                respond(status: 400, body: Data("invalid HTTP request".utf8))
            }
        }
    }

    private func receiveBody(request: HTTPRequest, bodyStart: Data.Index, missing: Int) {
        connection.receive(minimumIncompleteLength: missing, maximumLength: missing) { [weak self] data, _, isComplete, error in
            guard let self else { return }
            if error != nil {
                stop()
                return
            }
            if let data { buffer.append(data) }
            guard buffer.count - bodyStart >= request.bodyLength else {
                if isComplete {
                    respond(status: 400, body: Data("incomplete request body".utf8))
                } else {
                    receiveBody(
                        request: request,
                        bodyStart: bodyStart,
                        missing: request.bodyLength - (buffer.count - bodyStart)
                    )
                }
                return
            }
            perform(request: request, bodyStart: bodyStart)
        }
    }

    private func perform(request: HTTPRequest, bodyStart: Data.Index) {
        guard request.origin == nil || configuration.accepts(origin: request.origin) else {
            respond(status: 403, body: Data("origin rejected".utf8))
            return
        }
        guard request.path == configuration.diskPath || request.path == configuration.flushPath else {
            respond(status: 404, body: Data("route not found".utf8))
            return
        }
        if request.method == "OPTIONS" {
            guard request.isValidPreflight(configuration: configuration) else {
                respond(status: 400, body: Data("invalid preflight request".utf8))
                return
            }
            respond(status: 204, body: Data(), preflight: true)
            return
        }
        if request.path == configuration.flushPath {
            guard request.method == "POST", request.bodyLength == 0, request.query.isEmpty else {
                respond(status: 405, body: Data("method not allowed".utf8))
                return
            }
            store.flushAsync { [weak self] result in
                guard let session = self else { return }
                session.queue.async {
                    switch result {
                    case .success: session.respond(status: 204, body: Data())
                    case .failure(let error): session.respond(diskError: error)
                    }
                }
            }
            return
        }
        switch request.method {
        case "HEAD":
            guard request.bodyLength == 0, request.query.isEmpty else {
                respond(status: 400, body: Data("invalid HEAD request".utf8))
                return
            }
            respond(status: 200, body: Data(), diskSize: store.geometry.byteCount)
        case "GET":
            guard request.bodyLength == 0,
                  let offset = request.queryUInt64("offset"),
                  let length = request.queryUInt64("length"),
                  length <= UInt64(Int.max),
                  request.hasOnlyQueryItems(["offset", "length"]) else {
                respond(status: 400, body: Data("offset and length are required".utf8))
                return
            }
            store.readAsync(offset: offset, length: Int(length)) { [weak self] result in
                guard let session = self else { return }
                session.queue.async {
                    switch result {
                    case .success(let body): session.respond(status: 200, body: body)
                    case .failure(let error): session.respond(diskError: error)
                    }
                }
            }
        case "PUT":
            guard let offset = request.queryUInt64("offset"),
                  request.bodyLength > 0,
                  request.hasOnlyQueryItems(["offset"]) else {
                respond(status: 400, body: Data("offset and body are required".utf8))
                return
            }
            let body = buffer.subdata(in: bodyStart..<(bodyStart + request.bodyLength))
            store.writeAsync(offset: offset, data: body) { [weak self] result in
                guard let session = self else { return }
                session.queue.async {
                    switch result {
                    case .success: session.respond(status: 204, body: Data())
                    case .failure(let error): session.respond(diskError: error)
                    }
                }
            }
        default:
            respond(status: 405, body: Data("method not allowed".utf8))
        }
    }

    private func respond(
        status: Int,
        body: Data,
        diskSize: UInt64? = nil,
        preflight: Bool = false
    ) {
        guard !stopped else { return }
        let reason = HTTPResponse.reason(for: status)
        var response = Data("HTTP/1.1 \(status) \(reason)\r\nConnection: close\r\n".utf8)
        response.append(Data("Access-Control-Allow-Origin: \(configuration.allowedOrigin)\r\nVary: Origin\r\n".utf8))
        response.append(Data("Cache-Control: no-store\r\n".utf8))
        if preflight {
            response.append(Data("Access-Control-Allow-Methods: GET, HEAD, PUT, POST, OPTIONS\r\nAccess-Control-Allow-Headers: content-type\r\nAccess-Control-Max-Age: 60\r\n".utf8))
        }
        if let diskSize {
            response.append(Data("Access-Control-Expose-Headers: X-Lish-Disk-Size\r\n".utf8))
            response.append(Data("X-Lish-Disk-Size: \(diskSize)\r\n".utf8))
        }
        response.append(Data("Content-Length: \(body.count)\r\nContent-Type: application/octet-stream\r\n\r\n".utf8))
        response.append(body)
        connection.send(
            content: response,
            contentContext: .finalMessage,
            isComplete: true,
            completion: .contentProcessed { [weak self] _ in self?.stop() }
        )
    }

    private func respond(diskError error: Error) {
        let failure = DiskHTTPFailure(error: error)
        respond(status: failure.status, body: Data(failure.message.utf8))
    }

    private func parseRequest(headerEnd: Range<Data.Index>) throws -> HTTPRequest {
        let headerData = buffer.subdata(in: buffer.startIndex..<headerEnd.lowerBound)
        guard let text = String(data: headerData, encoding: .utf8) else { throw RequestError(status: 400, message: "invalid headers") }
        var lines = text.components(separatedBy: "\r\n")
        guard let requestLine = lines.first else { throw RequestError(status: 400, message: "missing request line") }
        lines.removeFirst()
        let parts = requestLine.split(separator: " ", omittingEmptySubsequences: true)
        guard parts.count == 3, parts[2] == "HTTP/1.1" else { throw RequestError(status: 400, message: "HTTP/1.1 is required") }
        var headers: [String: String] = [:]
        for line in lines {
            guard let separator = line.firstIndex(of: ":") else { throw RequestError(status: 400, message: "invalid header") }
            let name = line[..<separator].lowercased()
            let value = line[line.index(after: separator)...].trimmingCharacters(in: .whitespaces)
            guard headers[name] == nil else { throw RequestError(status: 400, message: "duplicate header") }
            headers[name] = value
        }
        guard headers["transfer-encoding"] == nil else { throw RequestError(status: 400, message: "chunked encoding is not supported") }
        let bodyLength = Int(headers["content-length"] ?? "0") ?? -1
        guard bodyLength >= 0 else { throw RequestError(status: 400, message: "invalid content length") }
        if parts[0] == "PUT" && headers["content-length"] == nil {
            throw RequestError(status: 411, message: "content length is required")
        }
        guard let target = URLComponents(string: String(parts[1])) else {
            throw RequestError(status: 400, message: "invalid request target")
        }
        return HTTPRequest(
            method: String(parts[0]),
            path: target.path,
            query: target.queryItems ?? [],
            origin: headers["origin"],
            accessControlRequestMethod: headers["access-control-request-method"],
            accessControlRequestHeaders: headers["access-control-request-headers"],
            bodyLength: bodyLength
        )
    }
}

private struct HTTPRequest {
    let method: String
    let path: String
    let query: [URLQueryItem]
    let origin: String?
    let accessControlRequestMethod: String?
    let accessControlRequestHeaders: String?
    let bodyLength: Int

    func queryUInt64(_ name: String) -> UInt64? {
        guard let value = query.first(where: { $0.name == name })?.value,
              query.filter({ $0.name == name }).count == 1 else { return nil }
        return UInt64(value)
    }

    func hasOnlyQueryItems(_ names: Set<String>) -> Bool {
        Set(query.map(\.name)) == names && query.count == names.count
    }

    func isValidPreflight(configuration: DiskHTTPServerConfiguration) -> Bool {
        guard bodyLength == 0,
              origin != nil,
              let requestedMethod = accessControlRequestMethod?
                .trimmingCharacters(in: .whitespacesAndNewlines)
                .uppercased(),
              let requestedHeaders else { return false }

        switch (path, requestedMethod) {
        case (configuration.diskPath, "HEAD"):
            return query.isEmpty && requestedHeaders.isEmpty
        case (configuration.diskPath, "GET"):
            return queryUInt64("offset") != nil
                && queryUInt64("length") != nil
                && hasOnlyQueryItems(["offset", "length"])
                && requestedHeaders.isEmpty
        case (configuration.diskPath, "PUT"):
            return queryUInt64("offset") != nil
                && hasOnlyQueryItems(["offset"])
                && requestedHeaders.isSubset(of: ["content-type"])
        case (configuration.flushPath, "POST"):
            return query.isEmpty && requestedHeaders.isEmpty
        default:
            return false
        }
    }

    private var requestedHeaders: Set<String>? {
        guard let value = accessControlRequestHeaders else { return [] }
        let names = value.split(separator: ",", omittingEmptySubsequences: false).map {
            $0.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        }
        guard names.allSatisfy({ !$0.isEmpty }) else { return nil }
        return Set(names)
    }
}

private struct RequestError: Error {
    let status: Int
    let message: String
}

private struct DiskHTTPFailure {
    let status: Int
    let message: String

    init(error: Error) {
        switch error as? DiskError {
        case .rangeOutOfBounds:
            status = 416
            message = "invalid disk range"
        case .emptyRange, .emptyWrite, .operationTooLarge:
            status = 400
            message = "invalid disk operation"
        default:
            status = 500
            message = "disk I/O failed"
        }
    }
}

private enum HTTPResponse {
    static func reason(for status: Int) -> String {
        switch status {
        case 200: return "OK"
        case 204: return "No Content"
        case 400: return "Bad Request"
        case 403: return "Forbidden"
        case 404: return "Not Found"
        case 405: return "Method Not Allowed"
        case 411: return "Length Required"
        case 413: return "Payload Too Large"
        case 416: return "Range Not Satisfiable"
        case 431: return "Request Header Fields Too Large"
        case 500: return "Internal Server Error"
        default: return "Error"
        }
    }
}

private extension Result where Failure == Error {
    var isSuccess: Bool {
        if case .success = self { return true }
        return false
    }
}
