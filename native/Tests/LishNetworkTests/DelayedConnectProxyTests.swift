import Darwin
import Foundation
import XCTest
@testable import LishNetwork

final class DelayedConnectProxyTests: XCTestCase {
    func testReturnsSuccessBeforeOpeningUpstreamConnection() throws {
        let server = try TCPReplyServer(reply: Data("server reply".utf8))
        defer { server.stop() }
        let proxy = try makeProxy(port: server.port)
        defer { proxy.stop() }
        let client = try UnixStreamClient(path: proxy.socketPath)
        defer { client.stop() }

        try client.send(connectRequest(port: server.port))
        let response = try client.receiveThrough(Data("\r\n\r\n".utf8))

        XCTAssertTrue(response.starts(with: Data("HTTP/1.1 200".utf8)))
        usleep(100_000)
        XCTAssertFalse(server.accepted)

        let payload = Data("client hello".utf8)
        try client.send(payload)
        XCTAssertTrue(waitUntil { server.received == payload })
        XCTAssertEqual(try client.receiveAvailable(), Data("server reply".utf8))
    }

    func testPreservesPayloadPipelinedAfterConnectRequest() throws {
        let server = try TCPReplyServer(reply: Data("reply".utf8))
        defer { server.stop() }
        let proxy = try makeProxy(port: server.port)
        defer { proxy.stop() }
        let client = try UnixStreamClient(path: proxy.socketPath)
        defer { client.stop() }

        let payload = Data([0x16, 0x03, 0x01, 0x00, 0x01, 0x00])
        var request = connectRequest(port: server.port)
        request.append(payload)
        try client.send(request)

        XCTAssertTrue(waitUntil { server.received == payload })
        let response = try client.receiveUntilClosed()
        XCTAssertTrue(response.starts(with: Data("HTTP/1.1 200".utf8)))
        XCTAssertEqual(response.suffix(5), Data("reply".utf8))
    }

    func testAcceptsFragmentedConnectRequest() throws {
        let server = try TCPReplyServer(reply: Data("ok".utf8))
        defer { server.stop() }
        let proxy = try makeProxy(port: server.port)
        defer { proxy.stop() }
        let client = try UnixStreamClient(path: proxy.socketPath)
        defer { client.stop() }

        let request = connectRequest(port: server.port)
        let split = request.count / 2
        try client.send(Data(request[..<split]))
        usleep(20_000)
        try client.send(Data(request[split...]))
        let response = try client.receiveThrough(Data("\r\n\r\n".utf8))
        XCTAssertTrue(response.starts(with: Data("HTTP/1.1 200".utf8)))
        XCTAssertFalse(server.accepted)

        try client.send(Data("payload".utf8))
        XCTAssertTrue(waitUntil { server.received == Data("payload".utf8) })
    }

    func testRejectsDisallowedConnectPort() throws {
        let proxy = try DelayedConnectProxy(configuration: .init(allowedPorts: [443]))
        defer { proxy.stop() }
        let client = try UnixStreamClient(path: proxy.socketPath)
        defer { client.stop() }

        try client.send(connectRequest(port: 22))
        let response = try client.receiveUntilClosed()
        XCTAssertTrue(response.starts(with: Data("HTTP/1.1 403".utf8)))
    }

    func testClosesIdleTunnelWithoutOpeningUpstreamConnection() throws {
        let server = try TCPReplyServer(reply: Data())
        defer { server.stop() }
        var configuration = DelayedConnectProxyConfiguration(
            allowedPorts: [server.port],
            allowLoopbackTargets: true
        )
        configuration.prePayloadTimeout = 0.1
        let proxy = try DelayedConnectProxy(configuration: configuration)
        defer { proxy.stop() }
        let client = try UnixStreamClient(path: proxy.socketPath)
        defer { client.stop() }

        try client.send(connectRequest(port: server.port))
        let response = try client.receiveUntilClosed()
        XCTAssertTrue(response.starts(with: Data("HTTP/1.1 200".utf8)))
        XCTAssertFalse(server.accepted)
    }

    func testClosesConnectionThatDoesNotSendConnectRequest() throws {
        var configuration = DelayedConnectProxyConfiguration()
        configuration.requestTimeout = 0.05
        let proxy = try DelayedConnectProxy(configuration: configuration)
        defer { proxy.stop() }
        let client = try UnixStreamClient(path: proxy.socketPath)
        defer { client.stop() }

        let response = try client.receiveUntilClosed()
        XCTAssertTrue(response.starts(with: Data("HTTP/1.1 408".utf8)))
    }

    func testRejectsLoopbackTargetByDefault() throws {
        let server = try TCPReplyServer(reply: Data())
        defer { server.stop() }
        let proxy = try DelayedConnectProxy(configuration: .init(allowedPorts: [server.port]))
        defer { proxy.stop() }
        let client = try UnixStreamClient(path: proxy.socketPath)
        defer { client.stop() }

        try client.send(connectRequest(port: server.port))
        let response = try client.receiveThrough(Data("\r\n\r\n".utf8))
        XCTAssertTrue(response.starts(with: Data("HTTP/1.1 200".utf8)))
        try client.send(Data("payload".utf8))
        XCTAssertEqual(try client.receiveUntilClosed(), Data())
        XCTAssertFalse(server.accepted)
    }

    func testRelaysResponseLargerThanBufferLimit() throws {
        let reply = Data((0..<(512 * 1024)).map { UInt8(truncatingIfNeeded: $0) })
        let server = try TCPReplyServer(reply: reply)
        defer { server.stop() }
        let proxy = try makeProxy(port: server.port)
        defer { proxy.stop() }
        let client = try UnixStreamClient(path: proxy.socketPath)
        defer { client.stop() }

        try client.send(connectRequest(port: server.port))
        _ = try client.receiveThrough(Data("\r\n\r\n".utf8))
        try client.send(Data("request".utf8))

        XCTAssertEqual(try client.receiveUntilClosed(), reply)
    }

    private func makeProxy(port: UInt16) throws -> DelayedConnectProxy {
        try DelayedConnectProxy(configuration: .init(
            allowedPorts: [port],
            allowLoopbackTargets: true
        ))
    }

    private func connectRequest(port: UInt16) -> Data {
        let request = "CONNECT 127.0.0.1:\(port) HTTP/1.1\r\n" +
            "Host: 127.0.0.1:\(port)\r\n\r\n"
        return Data(request.utf8)
    }

    private func waitUntil(_ predicate: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(2)
        repeat {
            if predicate() { return true }
            usleep(1_000)
        } while Date() < deadline
        return false
    }
}

private final class TCPReplyServer: @unchecked Sendable {
    let port: UInt16

    private let descriptor: Int32
    private let queue = DispatchQueue(label: "io.lish.network.delayed-connect-test-server")
    private let lock = NSLock()
    private var connectionDescriptor: Int32 = -1
    private var stopped = false
    private var acceptedConnection = false
    private var receivedData = Data()
    private let reply: Data

    var accepted: Bool {
        lock.lock()
        defer { lock.unlock() }
        return acceptedConnection
    }

    var received: Data {
        lock.lock()
        defer { lock.unlock() }
        return receivedData
    }

    init(reply: Data) throws {
        self.reply = reply
        let descriptor = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP)
        guard descriptor >= 0 else { throw currentPOSIXError() }
        self.descriptor = descriptor

        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = 0
        address.sin_addr.s_addr = inet_addr("127.0.0.1")
        let bindResult = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(descriptor, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard bindResult == 0, listen(descriptor, 1) == 0 else {
            close(descriptor)
            throw currentPOSIXError()
        }

        var boundAddress = sockaddr_in()
        var boundLength = socklen_t(MemoryLayout<sockaddr_in>.size)
        let nameResult = withUnsafeMutablePointer(to: &boundAddress) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                getsockname(descriptor, $0, &boundLength)
            }
        }
        guard nameResult == 0 else {
            close(descriptor)
            throw currentPOSIXError()
        }
        port = UInt16(bigEndian: boundAddress.sin_port)
        queue.async { [weak self] in self?.serve() }
    }

    func stop() {
        lock.lock()
        guard !stopped else {
            lock.unlock()
            return
        }
        stopped = true
        let connection = connectionDescriptor
        lock.unlock()
        if connection >= 0 { shutdown(connection, SHUT_RDWR) }
        close(descriptor)
    }

    private func serve() {
        let connection = accept(descriptor, nil, nil)
        guard connection >= 0 else { return }
        lock.lock()
        connectionDescriptor = connection
        acceptedConnection = true
        lock.unlock()

        var bytes = [UInt8](repeating: 0, count: 64 * 1024)
        let count = recv(connection, &bytes, bytes.count, 0)
        if count > 0 {
            lock.lock()
            receivedData = Data(bytes.prefix(count))
            lock.unlock()
            var remaining = reply
            while !remaining.isEmpty {
                let sent = remaining.withUnsafeBytes { buffer in
                    Darwin.send(connection, buffer.baseAddress, buffer.count, 0)
                }
                if sent <= 0 { break }
                remaining.removeFirst(sent)
            }
        }
        shutdown(connection, SHUT_RDWR)
        close(connection)
        lock.lock()
        connectionDescriptor = -1
        lock.unlock()
    }
}

private final class UnixStreamClient {
    private let descriptor: Int32
    private var pendingInput = Data()

    init(path: String) throws {
        let descriptor = socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw currentPOSIXError() }
        self.descriptor = descriptor

        var timeout = timeval(tv_sec: 3, tv_usec: 0)
        guard setsockopt(
            descriptor,
            SOL_SOCKET,
            SO_RCVTIMEO,
            &timeout,
            socklen_t(MemoryLayout.size(ofValue: timeout))
        ) == 0 else {
            close(descriptor)
            throw currentPOSIXError()
        }

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(path.utf8) + [0]
        guard pathBytes.count <= MemoryLayout.size(ofValue: address.sun_path) else {
            close(descriptor)
            throw POSIXError(.ENAMETOOLONG)
        }
        withUnsafeMutableBytes(of: &address.sun_path) { destination in
            destination.copyBytes(from: pathBytes)
        }
        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(
                    descriptor,
                    $0,
                    socklen_t(MemoryLayout<sockaddr_un>.size)
                )
            }
        }
        guard result == 0 else {
            close(descriptor)
            throw currentPOSIXError()
        }
    }

    func stop() {
        shutdown(descriptor, SHUT_RDWR)
        close(descriptor)
    }

    func send(_ data: Data) throws {
        var remaining = data
        while !remaining.isEmpty {
            let count = remaining.withUnsafeBytes { buffer in
                Darwin.send(descriptor, buffer.baseAddress, buffer.count, 0)
            }
            guard count > 0 else { throw currentPOSIXError() }
            remaining.removeFirst(count)
        }
    }

    func receiveThrough(_ delimiter: Data) throws -> Data {
        while true {
            if let range = pendingInput.range(of: delimiter) {
                let end = range.upperBound
                let result = Data(pendingInput[..<end])
                pendingInput.removeFirst(end)
                return result
            }
            try receiveMore()
        }
    }

    func receiveAvailable() throws -> Data {
        if pendingInput.isEmpty { try receiveMore() }
        let result = pendingInput
        pendingInput.removeAll()
        return result
    }

    func receiveUntilClosed() throws -> Data {
        while try receiveMore(allowEndOfStream: true) {}
        let result = pendingInput
        pendingInput.removeAll()
        return result
    }

    @discardableResult
    private func receiveMore(allowEndOfStream: Bool = false) throws -> Bool {
        var bytes = [UInt8](repeating: 0, count: 16 * 1024)
        let count = recv(descriptor, &bytes, bytes.count, 0)
        if count > 0 {
            pendingInput.append(contentsOf: bytes.prefix(count))
            return true
        }
        if count == 0, allowEndOfStream { return false }
        throw currentPOSIXError()
    }
}

private func currentPOSIXError() -> POSIXError {
    POSIXError(POSIXErrorCode(rawValue: errno) ?? .EINVAL)
}
