import Foundation
import Network

public enum RawEthernetServerError: Error, CustomStringConvertible {
    case invalidConfiguration(String)
    case listenerFailed(String)
    case stopped

    public var description: String {
        switch self {
        case .invalidConfiguration(let message): return message
        case .listenerFailed(let message): return message
        case .stopped: return "raw Ethernet server is stopped"
        }
    }
}

public struct RawEthernetServerConfiguration: Sendable {
    public static let protocolName = "lish.raw-ethernet.v1"

    public let capability: String
    public let allowedOrigin: String
    public let port: UInt16
    public let queueCapacity: UInt32
    public let disableHostLoopback: Bool
    public let allowRemoteConnections: Bool

    public init(
        capability: String,
        allowedOrigin: String,
        port: UInt16 = 0,
        queueCapacity: UInt32 = 256,
        disableHostLoopback: Bool = true,
        allowRemoteConnections: Bool = false
    ) throws {
        guard capability.utf8.count >= 32, capability.utf8.count <= 128,
              capability.utf8.allSatisfy({ byte in
                  (byte >= 48 && byte <= 57) ||
                  (byte >= 65 && byte <= 90) ||
                  (byte >= 97 && byte <= 122) || byte == 45 || byte == 95
              }) else {
            throw RawEthernetServerError.invalidConfiguration(
                "capability must contain 32 to 128 URL-safe ASCII characters"
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
            throw RawEthernetServerError.invalidConfiguration("allowedOrigin must be an HTTP origin")
        }
        guard queueCapacity > 0 else {
            throw RawEthernetServerError.invalidConfiguration("queueCapacity must be positive")
        }
        self.capability = capability
        self.allowedOrigin = allowedOrigin.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        self.port = port
        self.queueCapacity = queueCapacity
        self.disableHostLoopback = disableHostLoopback
        self.allowRemoteConnections = allowRemoteConnections
    }

    func accepts(
        subprotocols: [String],
        headers: [(name: String, value: String)]
    ) -> Bool {
        let offeredProtocols = Set(
            subprotocols.map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
        )
        let origins = headers
            .filter { $0.name.caseInsensitiveCompare("Origin") == .orderedSame }
            .map { $0.value.trimmingCharacters(in: .whitespacesAndNewlines) }
        return offeredProtocols.contains(Self.protocolName) &&
            offeredProtocols.contains(capability) &&
            origins == [allowedOrigin]
    }
}

public final class RawEthernetWebSocketServer: @unchecked Sendable {
    private enum Lifecycle: Equatable {
        case idle
        case starting
        case running
        case stopped
    }

    private final class State: @unchecked Sendable {
        let lock = NSLock()
        var listener: NWListener?
        var session: RawEthernetSession?
        var candidate: RawEthernetSession?
        var startCompletion: ((Result<UInt16, Error>) -> Void)?
        var lifecycle = Lifecycle.idle
    }

    private let configuration: RawEthernetServerConfiguration
    private let queue = DispatchQueue(label: "io.lish.network.websocket")
    private let authenticationQueue = DispatchQueue(label: "io.lish.network.websocket-auth")
    private let state = State()

    public init(configuration: RawEthernetServerConfiguration) {
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
                RawEthernetServerError.invalidConfiguration("server already started or stopped")
            ))
            return
        }
        state.lifecycle = .starting
        state.startCompletion = completion
        state.lock.unlock()

        do {
            let webSocket = NWProtocolWebSocket.Options(.version13)
            webSocket.autoReplyPing = true
            webSocket.maximumMessageSize = SlirpNetwork.maximumFrameSize
            let configuration = configuration
            webSocket.setClientRequestHandler(authenticationQueue) { protocols, headers in
                let valid = configuration.accepts(subprotocols: protocols, headers: headers)
                return NWProtocolWebSocket.Response(
                    status: valid ? .accept : .reject,
                    subprotocol: valid ? RawEthernetServerConfiguration.protocolName : nil
                )
            }

            let parameters = NWParameters(tls: nil, tcp: NWProtocolTCP.Options())
            parameters.allowLocalEndpointReuse = false
            parameters.acceptLocalOnly = !configuration.allowRemoteConnections
            parameters.defaultProtocolStack.applicationProtocols.insert(webSocket, at: 0)
            let requestedPort = configuration.port == 0
                ? NWEndpoint.Port.any
                : NWEndpoint.Port(rawValue: configuration.port)!
            let listener = try NWListener(using: parameters, on: requestedPort)
            listener.stateUpdateHandler = { [weak self] state in
                self?.handleListenerState(state)
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
        let session = state.session
        state.session = nil
        let candidate = state.candidate
        state.candidate = nil
        let completion = state.startCompletion
        state.startCompletion = nil
        state.lock.unlock()
        listener?.cancel()
        queue.async {
            session?.stop()
            candidate?.stop()
        }
        completion?(.failure(RawEthernetServerError.stopped))
    }

    public func statistics() throws -> SlirpNetworkStatistics {
        state.lock.lock()
        let active = state.session
        state.lock.unlock()
        guard let active else { throw RawEthernetServerError.stopped }
        return try active.statistics()
    }

    private func handleListenerState(_ state: NWListener.State) {
        switch state {
        case .ready:
            self.state.lock.lock()
            let port = self.state.listener?.port?.rawValue
            self.state.lock.unlock()
            if let port { finishStart(.success(port)) }
            else { finishStart(.failure(RawEthernetServerError.listenerFailed("listener has no port"))) }
        case .failed(let error):
            finishStart(.failure(RawEthernetServerError.listenerFailed(String(describing: error))))
            stop()
        default:
            break
        }
    }

    private func accept(_ connection: NWConnection) {
        do {
            let replacement = try RawEthernetSession(
                connection: connection,
                queue: queue,
                queueCapacity: configuration.queueCapacity,
                disableHostLoopback: configuration.disableHostLoopback,
                onReady: { [weak self] ready in self?.promote(ready) ?? false },
                onStop: { [weak self] stopped in self?.clear(stopped) }
            )
            state.lock.lock()
            guard state.listener != nil else {
                state.lock.unlock()
                replacement.stop()
                return
            }
            let previous = state.candidate
            state.candidate = replacement
            state.lock.unlock()
            replacement.start()
            previous?.stop()
        } catch {
            connection.cancel()
        }
    }

    private func promote(_ ready: RawEthernetSession) -> Bool {
        state.lock.lock()
        guard state.listener != nil, state.candidate === ready else {
            state.lock.unlock()
            return false
        }
        state.candidate = nil
        let previous = state.session
        state.session = ready
        state.lock.unlock()
        previous?.stop()
        return true
    }

    private func clear(_ stopped: RawEthernetSession) {
        state.lock.lock()
        if state.session === stopped { state.session = nil }
        if state.candidate === stopped { state.candidate = nil }
        state.lock.unlock()
    }

    private func finishStart(_ result: Result<UInt16, Error>) {
        state.lock.lock()
        guard state.lifecycle == .starting else {
            state.lock.unlock()
            return
        }
        switch result {
        case .success: state.lifecycle = .running
        case .failure: state.lifecycle = .stopped
        }
        let completion = state.startCompletion
        state.startCompletion = nil
        state.lock.unlock()
        completion?(result)
    }
}

private final class RawEthernetSession: @unchecked Sendable {
    private let connection: NWConnection
    private let queue: DispatchQueue
    private let network: SlirpNetwork
    private let drainSignal: SessionSignal
    private let onReady: @Sendable (RawEthernetSession) -> Bool
    private let onStop: @Sendable (RawEthernetSession) -> Void
    private var stopped = false
    private var sending = false

    private func logStop(_ reason: String) {
        guard ProcessInfo.processInfo.environment["LISH_NET_DIAGNOSTICS"] != nil else { return }
        let statistics = try? network.statistics()
        fputs("lish-network-session: \(reason) stats=\(String(describing: statistics))\n", stderr)
    }

    init(
        connection: NWConnection,
        queue: DispatchQueue,
        queueCapacity: UInt32,
        disableHostLoopback: Bool,
        onReady: @escaping @Sendable (RawEthernetSession) -> Bool,
        onStop: @escaping @Sendable (RawEthernetSession) -> Void
    ) throws {
        let drainSignal = SessionSignal()
        self.connection = connection
        self.queue = queue
        self.onReady = onReady
        self.onStop = onStop
        self.drainSignal = drainSignal
        self.network = try SlirpNetwork(
            queueCapacity: queueCapacity,
            disableHostLoopback: disableHostLoopback,
            outputQueue: queue,
            outputReady: { drainSignal.send() }
        )
        drainSignal.install { [weak self] in
            self?.sendNextFrame()
        }
    }

    func start() {
        connection.stateUpdateHandler = { [weak self] state in
            switch state {
            case .ready:
                guard let self else { return }
                if onReady(self) { receiveNextFrame() }
                else { stop() }
            case .failed(let error):
                self?.logStop("connection failed: \(error)")
                self?.stop()
            case .cancelled: self?.stop()
            default: break
            }
        }
        connection.start(queue: queue)
    }

    func stop() {
        guard finish() else { return }
        connection.cancel()
    }

    func statistics() throws -> SlirpNetworkStatistics {
        try network.statistics()
    }

    private func receiveNextFrame() {
        guard !stopped else { return }
        connection.receiveMessage { [weak self] data, context, _, error in
            guard let self else { return }
            if let error {
                logStop("receive failed: \(error)")
                stop()
                return
            }
            guard let metadata = context?
                .protocolMetadata(definition: NWProtocolWebSocket.definition)
                as? NWProtocolWebSocket.Metadata else {
                logStop("received an invalid WebSocket frame")
                stop()
                return
            }
            switch metadata.opcode {
            case .binary:
                guard let data,
                      !data.isEmpty,
                      data.count <= SlirpNetwork.maximumFrameSize else {
                    logStop("received an invalid Ethernet frame")
                    stop()
                    return
                }
                acceptOrRetry(data)
            case .close:
                close(code: metadata.closeCode)
            case .ping, .pong:
                receiveNextFrame()
            case .cont, .text:
                logStop("received unsupported WebSocket opcode \(metadata.opcode)")
                stop()
            @unknown default:
                logStop("received unknown WebSocket opcode")
                stop()
            }
        }
    }

    private func close(code: NWProtocolWebSocket.CloseCode) {
        guard finish() else { return }
        let metadata = NWProtocolWebSocket.Metadata(opcode: .close)
        metadata.closeCode = code
        let context = NWConnection.ContentContext(
            identifier: "lish.raw-ethernet.close",
            metadata: [metadata]
        )
        connection.send(
            content: nil,
            contentContext: context,
            isComplete: true,
            completion: .contentProcessed { [connection] _ in connection.cancel() }
        )
    }

    @discardableResult
    private func finish() -> Bool {
        guard !stopped else { return false }
        stopped = true
        drainSignal.invalidate()
        network.stop()
        onStop(self)
        return true
    }

    private func acceptOrRetry(_ frame: Data) {
        guard !stopped else { return }
        do {
            if try network.sendFromGuest(frame) {
                receiveNextFrame()
            } else {
                queue.asyncAfter(deadline: .now() + .milliseconds(1)) { [weak self] in
                    self?.acceptOrRetry(frame)
                }
            }
        } catch {
            logStop("guest frame delivery failed: \(error)")
            stop()
        }
    }

    private func sendNextFrame() {
        guard !stopped, !sending else { return }
        do {
            guard let frame = try network.nextFrameForGuest() else { return }
            sending = true
            let metadata = NWProtocolWebSocket.Metadata(opcode: .binary)
            let context = NWConnection.ContentContext(
                identifier: "lish.raw-ethernet",
                metadata: [metadata]
            )
            connection.send(
                content: frame,
                contentContext: context,
                isComplete: true,
                completion: .contentProcessed { [weak self] error in
                    guard let self else { return }
                    queue.async {
                        self.sending = false
                        if let error {
                            self.logStop("send failed: \(error)")
                            self.stop()
                        } else {
                            self.sendNextFrame()
                        }
                    }
                }
            )
        } catch {
            logStop("host frame delivery failed: \(error)")
            stop()
        }
    }
}

private final class SessionSignal: @unchecked Sendable {
    private let lock = NSLock()
    private var callback: (@Sendable () -> Void)?

    func install(_ callback: @escaping @Sendable () -> Void) {
        lock.lock()
        self.callback = callback
        lock.unlock()
    }

    func send() {
        lock.lock()
        let current = callback
        lock.unlock()
        current?()
    }

    func invalidate() {
        lock.lock()
        callback = nil
        lock.unlock()
    }
}
