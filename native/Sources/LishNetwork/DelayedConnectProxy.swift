import Darwin
import Foundation
import Network

struct DelayedConnectProxyConfiguration: Sendable {
    var allowedPorts: Set<UInt16> = [443]
    var allowLoopbackTargets = false
    var maximumConnections = 64
    var maximumHeaderBytes = 16 * 1024
    var maximumBufferedBytes = 64 * 1024
    var requestTimeout: TimeInterval = 10
    var prePayloadTimeout: TimeInterval = 60
    var connectTimeout: TimeInterval = 15
}

enum DelayedConnectProxyError: Error, CustomStringConvertible {
    case invalidConfiguration(String)
    case listenerFailed(String)

    var description: String {
        switch self {
        case .invalidConfiguration(let message): return message
        case .listenerFailed(let message): return message
        }
    }
}

final class DelayedConnectProxy: @unchecked Sendable {
    static let guestAddress = "10.0.2.4"
    static let guestPort: UInt16 = 3128

    let socketPath: String

    private struct ListenerResources {
        let descriptor: Int32
        let socketPath: String
        let directoryPath: String
    }

    private let configuration: DelayedConnectProxyConfiguration
    private let queue = DispatchQueue(label: "io.lish.network.delayed-connect-proxy")
    private let lifecycle = NSLock()
    private let listenerDescriptor: Int32
    private let directoryPath: String
    private var listenerSource: DispatchSourceRead?
    private var tunnels: [Int32: DelayedConnectTunnel] = [:]
    private var stopped = false

    init(configuration: DelayedConnectProxyConfiguration = .init()) throws {
        guard !configuration.allowedPorts.isEmpty,
              configuration.maximumConnections > 0,
              configuration.maximumHeaderBytes > 0,
              configuration.maximumBufferedBytes > 0,
              configuration.requestTimeout > 0,
              configuration.prePayloadTimeout > 0,
              configuration.connectTimeout > 0 else {
            throw DelayedConnectProxyError.invalidConfiguration(
                "delayed CONNECT proxy limits must be positive"
            )
        }

        let resources = try Self.makeListener()
        self.configuration = configuration
        listenerDescriptor = resources.descriptor
        socketPath = resources.socketPath
        directoryPath = resources.directoryPath

        let source = DispatchSource.makeReadSource(
            fileDescriptor: resources.descriptor,
            queue: queue
        )
        listenerSource = source
        source.setEventHandler { [weak self] in
            self?.acceptConnections()
        }
        source.setCancelHandler { [descriptor = resources.descriptor] in
            close(descriptor)
        }
        source.resume()
    }

    deinit {
        stop()
    }

    func stop() {
        lifecycle.lock()
        guard !stopped else {
            lifecycle.unlock()
            return
        }
        stopped = true
        lifecycle.unlock()

        queue.sync {
            listenerSource?.cancel()
            listenerSource = nil
            let active = Array(tunnels.values)
            tunnels.removeAll()
            for tunnel in active {
                tunnel.stop()
            }
            unlink(socketPath)
            rmdir(directoryPath)
        }
    }

    private func acceptConnections() {
        while true {
            let descriptor = accept(listenerDescriptor, nil, nil)
            if descriptor < 0 {
                if errno == EINTR { continue }
                if errno == EAGAIN || errno == EWOULDBLOCK { return }
                return
            }
            guard tunnels.count < configuration.maximumConnections,
                  Self.configureStreamSocket(descriptor) else {
                close(descriptor)
                continue
            }

            let tunnel = DelayedConnectTunnel(
                guestDescriptor: descriptor,
                configuration: configuration,
                queue: queue,
                onStop: { [weak self] stoppedDescriptor in
                    self?.tunnels.removeValue(forKey: stoppedDescriptor)
                }
            )
            tunnels[descriptor] = tunnel
            tunnel.start()
        }
    }

    private static func makeListener() throws -> ListenerResources {
        let template = (NSTemporaryDirectory() as NSString)
            .appendingPathComponent("lish-proxy.XXXXXX")
        var templateBytes = Array(template.utf8CString)
        guard let directory = templateBytes.withUnsafeMutableBufferPointer({ buffer in
            mkdtemp(buffer.baseAddress)
        }) else {
            throw DelayedConnectProxyError.listenerFailed(
                "unable to create the delayed CONNECT proxy directory: \(posixMessage())"
            )
        }

        let directoryPath = String(cString: directory)
        let socketPath = (directoryPath as NSString).appendingPathComponent("proxy.sock")
        var address = sockaddr_un()
        let pathBytes = Array(socketPath.utf8) + [0]
        let pathCapacity = MemoryLayout.size(ofValue: address.sun_path)
        guard pathBytes.count <= pathCapacity else {
            rmdir(directoryPath)
            throw DelayedConnectProxyError.listenerFailed(
                "delayed CONNECT proxy Unix socket path is too long"
            )
        }

        let descriptor = socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else {
            rmdir(directoryPath)
            throw DelayedConnectProxyError.listenerFailed(
                "unable to create the delayed CONNECT proxy socket: \(posixMessage())"
            )
        }

        var keepDescriptor = false
        defer {
            if !keepDescriptor {
                close(descriptor)
                unlink(socketPath)
                rmdir(directoryPath)
            }
        }

        address.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &address.sun_path) { destination in
            destination.copyBytes(from: pathBytes)
        }
        let bindResult = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { socketAddress in
                Darwin.bind(
                    descriptor,
                    socketAddress,
                    socklen_t(MemoryLayout<sockaddr_un>.size)
                )
            }
        }
        guard bindResult == 0,
              configureStreamSocket(descriptor),
              listen(descriptor, 64) == 0 else {
            throw DelayedConnectProxyError.listenerFailed(
                "unable to bind the delayed CONNECT proxy socket: \(posixMessage())"
            )
        }

        keepDescriptor = true
        return ListenerResources(
            descriptor: descriptor,
            socketPath: socketPath,
            directoryPath: directoryPath
        )
    }

    private static func configureStreamSocket(_ descriptor: Int32) -> Bool {
        let statusFlags = fcntl(descriptor, F_GETFL, 0)
        guard statusFlags >= 0,
              fcntl(descriptor, F_SETFL, statusFlags | O_NONBLOCK) == 0 else {
            return false
        }
        let descriptorFlags = fcntl(descriptor, F_GETFD, 0)
        guard descriptorFlags >= 0,
              fcntl(descriptor, F_SETFD, descriptorFlags | FD_CLOEXEC) == 0 else {
            return false
        }
        var noSignal: Int32 = 1
        return setsockopt(
            descriptor,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            &noSignal,
            socklen_t(MemoryLayout.size(ofValue: noSignal))
        ) == 0
    }

    private static func posixMessage() -> String {
        String(cString: strerror(errno))
    }
}

private struct ConnectTarget: Equatable, Sendable {
    let host: String
    let port: UInt16
}

private struct ConnectRequestError: Error {
    let status: Int
    let reason: String
}

private enum ConnectRequestParser {
    static func parse(_ data: Data, allowedPorts: Set<UInt16>) throws -> ConnectTarget {
        guard data.allSatisfy({ byte in
            byte == 9 || byte == 10 || byte == 13 || (byte >= 32 && byte <= 126)
        }), let request = String(data: data, encoding: .utf8) else {
            throw ConnectRequestError(status: 400, reason: "Bad Request")
        }

        let lines = request.components(separatedBy: "\r\n")
        guard let requestLine = lines.first else {
            throw ConnectRequestError(status: 400, reason: "Bad Request")
        }
        let words = requestLine.split(separator: " ", omittingEmptySubsequences: false)
        guard words.count == 3 else {
            throw ConnectRequestError(status: 400, reason: "Bad Request")
        }
        guard words[0] == "CONNECT" else {
            throw ConnectRequestError(status: 405, reason: "Method Not Allowed")
        }
        guard words[2] == "HTTP/1.1" else {
            throw ConnectRequestError(status: 505, reason: "HTTP Version Not Supported")
        }

        let authority = String(words[1])
        let target = try parseAuthority(authority)
        guard allowedPorts.contains(target.port) else {
            throw ConnectRequestError(status: 403, reason: "Forbidden")
        }

        var hostHeaders: [String] = []
        for line in lines.dropFirst() {
            guard !line.isEmpty, line.first != " ", line.first != "\t",
                  let colon = line.firstIndex(of: ":") else {
                throw ConnectRequestError(status: 400, reason: "Bad Request")
            }
            let name = line[..<colon]
            guard !name.isEmpty, name.allSatisfy(isHeaderNameByte) else {
                throw ConnectRequestError(status: 400, reason: "Bad Request")
            }
            let value = line[line.index(after: colon)...]
                .trimmingCharacters(in: .whitespaces)
            switch name.lowercased() {
            case "host":
                hostHeaders.append(value)
            case "content-length" where value != "0":
                throw ConnectRequestError(status: 400, reason: "Bad Request")
            case "transfer-encoding":
                throw ConnectRequestError(status: 400, reason: "Bad Request")
            default:
                break
            }
        }
        guard hostHeaders.count == 1,
              hostHeaders[0].caseInsensitiveCompare(authority) == .orderedSame else {
            throw ConnectRequestError(status: 400, reason: "Bad Request")
        }
        return target
    }

    private static func parseAuthority(_ authority: String) throws -> ConnectTarget {
        guard !authority.isEmpty, !authority.contains("@"), !authority.hasPrefix("[") else {
            throw ConnectRequestError(status: 400, reason: "Bad Request")
        }
        let parts = authority.split(separator: ":", omittingEmptySubsequences: false)
        guard parts.count == 2,
              let port = UInt16(parts[1]), port != 0 else {
            throw ConnectRequestError(status: 400, reason: "Bad Request")
        }
        let host = String(parts[0])
        guard isValidIPv4Address(host) || isValidHostname(host) else {
            throw ConnectRequestError(status: 400, reason: "Bad Request")
        }
        return ConnectTarget(host: host, port: port)
    }

    private static func isValidIPv4Address(_ value: String) -> Bool {
        var address = in_addr()
        return value.withCString { inet_pton(AF_INET, $0, &address) == 1 }
    }

    private static func isValidHostname(_ value: String) -> Bool {
        let hostname = value.hasSuffix(".") ? String(value.dropLast()) : value
        guard !hostname.isEmpty, hostname.utf8.count <= 253 else { return false }
        return hostname.split(separator: ".", omittingEmptySubsequences: false).allSatisfy { label in
            guard !label.isEmpty, label.utf8.count <= 63,
                  let first = label.utf8.first, let last = label.utf8.last,
                  isAlphaNumeric(first), isAlphaNumeric(last) else {
                return false
            }
            return label.utf8.allSatisfy { isAlphaNumeric($0) || $0 == 45 }
        }
    }

    private static func isAlphaNumeric(_ byte: UInt8) -> Bool {
        (byte >= 48 && byte <= 57) ||
            (byte >= 65 && byte <= 90) ||
            (byte >= 97 && byte <= 122)
    }

    private static func isHeaderNameByte(_ character: Character) -> Bool {
        character.asciiValue.map { byte in
            isAlphaNumeric(byte) || "!#$%&'*+-.^_`|~".utf8.contains(byte)
        } ?? false
    }
}

private struct ResolutionFailure: Error, Sendable {}

private enum IPv4Resolver {
    static func resolve(
        host: String,
        allowLoopback: Bool
    ) -> Result<[String], ResolutionFailure> {
        var hints = addrinfo(
            ai_flags: AI_ADDRCONFIG,
            ai_family: AF_INET,
            ai_socktype: SOCK_STREAM,
            ai_protocol: IPPROTO_TCP,
            ai_addrlen: 0,
            ai_canonname: nil,
            ai_addr: nil,
            ai_next: nil
        )
        var result: UnsafeMutablePointer<addrinfo>?
        guard getaddrinfo(host, nil, &hints, &result) == 0, let first = result else {
            return .failure(ResolutionFailure())
        }
        defer { freeaddrinfo(first) }

        var addresses: [String] = []
        var current: UnsafeMutablePointer<addrinfo>? = first
        while let entry = current {
            defer { current = entry.pointee.ai_next }
            guard entry.pointee.ai_family == AF_INET,
                  let rawAddress = entry.pointee.ai_addr else {
                continue
            }
            let socketAddress = UnsafeRawPointer(rawAddress)
                .assumingMemoryBound(to: sockaddr_in.self)
            let address = socketAddress.pointee.sin_addr
            guard isAllowed(address, allowLoopback: allowLoopback) else { continue }

            var value = address
            var buffer = [CChar](repeating: 0, count: Int(INET_ADDRSTRLEN))
            guard inet_ntop(AF_INET, &value, &buffer, socklen_t(buffer.count)) != nil else {
                continue
            }
            let text = String(
                decoding: buffer.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) },
                as: UTF8.self
            )
            if !addresses.contains(text) { addresses.append(text) }
        }
        return addresses.isEmpty ? .failure(ResolutionFailure()) : .success(addresses)
    }

    private static func isAllowed(_ address: in_addr, allowLoopback: Bool) -> Bool {
        let value = UInt32(bigEndian: address.s_addr)
        let first = UInt8(truncatingIfNeeded: value >> 24)
        let second = UInt8(truncatingIfNeeded: value >> 16)
        if first == 127 { return allowLoopback }
        if first == 0 || first >= 224 { return false }
        if first == 169 && second == 254 { return false }
        return true
    }
}

private final class DelayedConnectTunnel: @unchecked Sendable {
    private enum Phase {
        case readingRequest
        case armed
        case connecting
        case tunneling
        case closed
    }

    private static let resolverQueue = DispatchQueue(
        label: "io.lish.network.delayed-connect-resolver",
        attributes: .concurrent
    )
    private static let readSize = 16 * 1024

    private let guestDescriptor: Int32
    private let configuration: DelayedConnectProxyConfiguration
    private let queue: DispatchQueue
    private let onStop: @Sendable (Int32) -> Void
    private var guestReadSource: DispatchSourceRead?
    private var guestReadSuspended = false
    private var guestWriteSource: DispatchSourceWrite?
    private var phase = Phase.readingRequest
    private var requestBuffer = Data()
    private var guestOutput = Data()
    private var upstreamOutput = Data()
    private var target: ConnectTarget?
    private var resolvedAddresses: [String]?
    private var nextAddressIndex = 0
    private var upstream: NWConnection?
    private var upstreamSendPending = false
    private var upstreamReceivePending = false
    private var guestInputComplete = false
    private var upstreamInputComplete = false
    private var upstreamOutputComplete = false
    private var guestOutputComplete = false
    private var receivedTunnelPayload = false
    private var closeAfterGuestOutput = false
    private var connectGeneration = 0

    init(
        guestDescriptor: Int32,
        configuration: DelayedConnectProxyConfiguration,
        queue: DispatchQueue,
        onStop: @escaping @Sendable (Int32) -> Void
    ) {
        self.guestDescriptor = guestDescriptor
        self.configuration = configuration
        self.queue = queue
        self.onStop = onStop
    }

    func start() {
        let source = DispatchSource.makeReadSource(fileDescriptor: guestDescriptor, queue: queue)
        guestReadSource = source
        source.setEventHandler { [weak self] in
            self?.readGuest()
        }
        source.resume()
        armRequestTimeout()
    }

    func stop() {
        guard phase != .closed else { return }
        phase = .closed
        connectGeneration += 1
        upstream?.stateUpdateHandler = nil
        upstream?.cancel()
        upstream = nil
        cancelGuestRead()
        guestWriteSource?.cancel()
        guestWriteSource = nil
        close(guestDescriptor)
        onStop(guestDescriptor)
    }

    private func readGuest() {
        while phase != .closed && !guestReadSuspended {
            let readCapacity: Int
            switch phase {
            case .armed, .connecting, .tunneling:
                readCapacity = min(
                    Self.readSize,
                    configuration.maximumBufferedBytes - upstreamOutput.count
                )
            case .readingRequest:
                readCapacity = Self.readSize
            case .closed:
                return
            }
            if readCapacity == 0 {
                suspendGuestRead()
                return
            }
            var bytes = [UInt8](repeating: 0, count: readCapacity)
            let count = recv(guestDescriptor, &bytes, bytes.count, 0)
            if count > 0 {
                processGuestData(Data(bytes.prefix(count)))
                continue
            }
            if count == 0 {
                log("guest input reached EOF")
                handleGuestInputComplete()
                return
            }
            if errno == EINTR { continue }
            if errno == EAGAIN || errno == EWOULDBLOCK { return }
            log("guest read failed: \(String(cString: strerror(errno)))")
            stop()
            return
        }
    }

    private func processGuestData(_ data: Data) {
        switch phase {
        case .readingRequest:
            requestBuffer.append(data)
            guard let delimiter = requestBuffer.range(of: Data("\r\n\r\n".utf8)) else {
                if requestBuffer.count > configuration.maximumHeaderBytes {
                    respondAndClose(status: 431, reason: "Request Header Fields Too Large")
                }
                return
            }
            guard delimiter.upperBound <= configuration.maximumHeaderBytes else {
                respondAndClose(status: 431, reason: "Request Header Fields Too Large")
                return
            }
            let header = Data(requestBuffer[..<delimiter.lowerBound])
            let payload = Data(requestBuffer[delimiter.upperBound...])
            requestBuffer.removeAll(keepingCapacity: false)
            do {
                let parsed = try ConnectRequestParser.parse(
                    header,
                    allowedPorts: configuration.allowedPorts
                )
                target = parsed
                phase = .armed
                log("CONNECT acknowledged")
                enqueueGuest(Data("HTTP/1.1 200 Connection Established\r\n\r\n".utf8))
                resolve(parsed)
                armPrePayloadTimeout()
                if !payload.isEmpty { bufferForUpstream(payload) }
            } catch let error as ConnectRequestError {
                log("CONNECT rejected with HTTP \(error.status)")
                respondAndClose(status: error.status, reason: error.reason)
            } catch {
                respondAndClose(status: 400, reason: "Bad Request")
            }
        case .armed, .connecting, .tunneling:
            bufferForUpstream(data)
        case .closed:
            break
        }
    }

    private func resolve(_ target: ConnectTarget) {
        let allowLoopback = configuration.allowLoopbackTargets
        Self.resolverQueue.async { [weak self, queue] in
            guard let tunnel = self else { return }
            let result = IPv4Resolver.resolve(host: target.host, allowLoopback: allowLoopback)
            queue.async {
                tunnel.resolved(result)
            }
        }
    }

    private func resolved(_ result: Result<[String], ResolutionFailure>) {
        guard phase == .armed else { return }
        switch result {
        case .success(let addresses):
            resolvedAddresses = addresses
            maybeConnect()
        case .failure:
            log("target resolution failed")
            closeAfterPendingGuestOutput()
        }
    }

    private func bufferForUpstream(_ data: Data) {
        guard !data.isEmpty else { return }
        if !receivedTunnelPayload {
            log("received first tunnel payload (\(data.count) bytes)")
        }
        receivedTunnelPayload = true
        guard upstreamOutput.count + data.count <= configuration.maximumBufferedBytes else {
            log("guest-to-upstream buffer limit exceeded")
            stop()
            return
        }
        upstreamOutput.append(data)
        if upstreamOutput.count == configuration.maximumBufferedBytes {
            suspendGuestRead()
        }
        if phase == .armed {
            maybeConnect()
        } else if phase == .tunneling {
            sendUpstream()
        }
    }

    private func maybeConnect() {
        guard phase == .armed, receivedTunnelPayload, resolvedAddresses != nil else { return }
        phase = .connecting
        connectNextAddress()
    }

    private func connectNextAddress() {
        guard phase == .connecting, let addresses = resolvedAddresses,
              nextAddressIndex < addresses.count, let target else {
            log("all upstream connection attempts failed")
            closeAfterPendingGuestOutput()
            return
        }
        let address = addresses[nextAddressIndex]
        nextAddressIndex += 1

        let parameters = NWParameters.tcp
        parameters.preferNoProxies = true
        let connection = NWConnection(
            host: NWEndpoint.Host(address),
            port: NWEndpoint.Port(rawValue: target.port)!,
            using: parameters
        )
        upstream = connection
        connectGeneration += 1
        let generation = connectGeneration
        connection.stateUpdateHandler = { [weak self, weak connection] state in
            guard let self, let connection, self.upstream === connection else { return }
            switch state {
            case .ready:
                self.log("upstream connection ready")
                self.phase = .tunneling
                self.sendUpstream()
                self.receiveUpstream()
            case .failed(let error):
                self.log("upstream connection failed: \(error)")
                self.upstream = nil
                connection.cancel()
                self.connectNextAddress()
            case .cancelled where self.phase != .closed:
                self.upstream = nil
                self.connectNextAddress()
            default:
                break
            }
        }
        connection.start(queue: queue)
        queue.asyncAfter(deadline: .now() + configuration.connectTimeout) { [weak self] in
            guard let self, self.phase == .connecting,
                  self.connectGeneration == generation,
                  self.upstream === connection else { return }
            self.log("upstream connection timed out")
            self.upstream = nil
            connection.cancel()
            self.connectNextAddress()
        }
    }

    private func sendUpstream() {
        guard phase == .tunneling, !upstreamSendPending, let upstream else { return }
        if upstreamOutput.isEmpty {
            resumeGuestReadIfPossible()
            if guestInputComplete && !upstreamOutputComplete {
                upstreamOutputComplete = true
                upstreamSendPending = true
                upstream.send(
                    content: nil,
                    contentContext: .finalMessage,
                    isComplete: true,
                    completion: .contentProcessed { [weak self] error in
                        guard let self else { return }
                        self.upstreamSendPending = false
                        if let error {
                            self.log("upstream final send failed: \(error)")
                            self.stop()
                        }
                        else { self.finishIfComplete() }
                    }
                )
            }
            return
        }

        let content = upstreamOutput
        upstreamOutput.removeAll(keepingCapacity: true)
        upstreamSendPending = true
        resumeGuestReadIfPossible()
        upstream.send(content: content, completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            self.upstreamSendPending = false
            if let error {
                self.log("upstream send failed: \(error)")
                self.stop()
            }
            else { self.sendUpstream() }
        })
    }

    private func receiveUpstream() {
        guard phase == .tunneling, !upstreamReceivePending, !upstreamInputComplete,
              guestOutput.count < configuration.maximumBufferedBytes,
              let upstream else { return }
        let receiveCapacity = min(
            Self.readSize,
            configuration.maximumBufferedBytes - guestOutput.count
        )
        upstreamReceivePending = true
        upstream.receive(
            minimumIncompleteLength: 1,
            maximumLength: receiveCapacity
        ) { [weak self] data, _, isComplete, error in
            guard let self else { return }
            self.upstreamReceivePending = false
            if let data, !data.isEmpty {
                guard self.guestOutput.count + data.count <=
                    self.configuration.maximumBufferedBytes else {
                    self.log("upstream-to-guest buffer limit exceeded")
                    self.stop()
                    return
                }
                self.enqueueGuest(data)
            }
            if let error {
                self.log("upstream receive failed: \(error)")
            }
            if isComplete {
                self.log("upstream input reached EOF")
            }
            if error != nil || isComplete {
                self.upstreamInputComplete = true
                self.completeGuestOutputWhenDrained()
            } else {
                self.receiveUpstream()
            }
        }
    }

    private func enqueueGuest(_ data: Data) {
        guard phase != .closed else { return }
        guestOutput.append(data)
        flushGuestOutput()
    }

    private func flushGuestOutput() {
        guard phase != .closed else { return }
        while !guestOutput.isEmpty {
            let written = guestOutput.withUnsafeBytes { bytes in
                Darwin.send(guestDescriptor, bytes.baseAddress, bytes.count, 0)
            }
            if written > 0 {
                guestOutput.removeFirst(written)
                continue
            }
            if written < 0 && errno == EINTR { continue }
            if written < 0 && (errno == EAGAIN || errno == EWOULDBLOCK) {
                armGuestWrite()
                return
            }
            log("guest write failed: \(String(cString: strerror(errno)))")
            stop()
            return
        }

        guestWriteSource?.cancel()
        guestWriteSource = nil
        if closeAfterGuestOutput {
            stop()
            return
        }
        if upstreamInputComplete { completeGuestOutputWhenDrained() }
        receiveUpstream()
    }

    private func armGuestWrite() {
        guard guestWriteSource == nil else { return }
        let source = DispatchSource.makeWriteSource(fileDescriptor: guestDescriptor, queue: queue)
        guestWriteSource = source
        source.setEventHandler { [weak self] in
            self?.flushGuestOutput()
        }
        source.resume()
    }

    private func respondAndClose(status: Int, reason: String) {
        suspendGuestRead()
        closeAfterGuestOutput = true
        enqueueGuest(Data("HTTP/1.1 \(status) \(reason)\r\nConnection: close\r\n\r\n".utf8))
    }

    private func closeAfterPendingGuestOutput() {
        suspendGuestRead()
        closeAfterGuestOutput = true
        if guestOutput.isEmpty { stop() }
    }

    private func handleGuestInputComplete() {
        guestInputComplete = true
        cancelGuestRead()
        switch phase {
        case .readingRequest, .armed:
            stop()
        case .connecting:
            break
        case .tunneling:
            sendUpstream()
        case .closed:
            break
        }
    }

    private func completeGuestOutputWhenDrained() {
        guard upstreamInputComplete, guestOutput.isEmpty, !guestOutputComplete else { return }
        guestOutputComplete = true
        shutdown(guestDescriptor, SHUT_WR)
        finishIfComplete()
    }

    private func finishIfComplete() {
        if guestInputComplete && upstreamInputComplete && !upstreamSendPending && guestOutput.isEmpty {
            stop()
        }
    }

    private func armPrePayloadTimeout() {
        queue.asyncAfter(deadline: .now() + configuration.prePayloadTimeout) { [weak self] in
            guard let self, self.phase == .armed, !self.receivedTunnelPayload else { return }
            self.closeAfterPendingGuestOutput()
        }
    }

    private func armRequestTimeout() {
        queue.asyncAfter(deadline: .now() + configuration.requestTimeout) { [weak self] in
            guard let self, self.phase == .readingRequest else { return }
            self.respondAndClose(status: 408, reason: "Request Timeout")
        }
    }

    private func log(_ message: String) {
        guard ProcessInfo.processInfo.environment["LISH_NET_DIAGNOSTICS"] != nil else { return }
        let authority = target.map { "\($0.host):\($0.port)" } ?? "unknown"
        fputs("lish-connect-proxy: \(message) target=\(authority)\n", stderr)
    }

    private func suspendGuestRead() {
        guard let source = guestReadSource, !guestReadSuspended else { return }
        source.suspend()
        guestReadSuspended = true
    }

    private func resumeGuestReadIfPossible() {
        guard guestReadSuspended,
              upstreamOutput.count < configuration.maximumBufferedBytes,
              let source = guestReadSource else { return }
        guestReadSuspended = false
        source.resume()
    }

    private func cancelGuestRead() {
        guard let source = guestReadSource else { return }
        if guestReadSuspended {
            source.resume()
            guestReadSuspended = false
        }
        source.cancel()
        guestReadSource = nil
    }
}
