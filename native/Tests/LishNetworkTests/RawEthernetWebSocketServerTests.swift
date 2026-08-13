import Foundation
import XCTest
@testable import LishNetwork

final class RawEthernetWebSocketServerTests: XCTestCase {
    private let capability = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    private let origin = "http://127.0.0.1:4173"

    func testAuthenticatedWebSocketCarriesDHCP() throws {
        let configuration = try RawEthernetServerConfiguration(
            capability: capability,
            allowedOrigin: origin
        )
        let server = RawEthernetWebSocketServer(configuration: configuration)
        defer { server.stop() }
        let port = try start(server)
        let connection = makeConnection(port: port, capability: capability, origin: origin)
        defer { connection.stop() }

        let response = expectation(description: "DHCP offer")
        receiveOffer(connection.task, expectation: response)
        connection.task.resume()
        connection.task.send(.data(DHCPPacket.discover())) { error in
            if let error { XCTFail("WebSocket send failed: \(error)") }
        }
        wait(for: [response], timeout: 3)
    }

    func testAuthenticatedConnectionReplacesLiveSession() throws {
        let configuration = try RawEthernetServerConfiguration(
            capability: capability,
            allowedOrigin: origin
        )
        let server = RawEthernetWebSocketServer(configuration: configuration)
        defer { server.stop() }
        let port = try start(server)
        let first = makeConnection(port: port, capability: capability, origin: origin)
        defer { first.stop() }

        let firstOffer = expectation(description: "first DHCP offer")
        receiveOffer(first.task, expectation: firstOffer)
        first.task.resume()
        first.task.send(.data(DHCPPacket.discover())) { error in
            if let error { XCTFail("First WebSocket send failed: \(error)") }
        }
        wait(for: [firstOffer], timeout: 3)

        let replacement = makeConnection(port: port, capability: capability, origin: origin)
        defer { replacement.stop() }
        let replacementOffer = expectation(description: "replacement DHCP offer")
        receiveOffer(replacement.task, expectation: replacementOffer)
        replacement.task.resume()
        replacement.task.send(.data(DHCPPacket.discover())) { error in
            if let error { XCTFail("Replacement WebSocket send failed: \(error)") }
        }
        wait(for: [replacementOffer], timeout: 3)
    }

    func testClientCloseCompletesWebSocketHandshake() throws {
        let configuration = try RawEthernetServerConfiguration(
            capability: capability,
            allowedOrigin: origin
        )
        let server = RawEthernetWebSocketServer(configuration: configuration)
        defer { server.stop() }
        let port = try start(server)
        let connection = makeConnection(port: port, capability: capability, origin: origin)

        let offer = expectation(description: "DHCP offer before close")
        receiveOffer(connection.task, expectation: offer)
        connection.task.resume()
        connection.task.send(.data(DHCPPacket.discover())) { error in
            if let error { XCTFail("WebSocket send failed: \(error)") }
        }
        wait(for: [offer], timeout: 3)

        let closed = expectation(description: "WebSocket close response")
        connection.onClose { code in
            XCTAssertEqual(code, .goingAway)
            closed.fulfill()
        }
        connection.close(with: .goingAway)
        wait(for: [closed], timeout: 3)
        connection.invalidate()
    }

    func testHandshakeAcceptsHeaderWhitespace() throws {
        let configuration = try RawEthernetServerConfiguration(
            capability: capability,
            allowedOrigin: origin
        )
        XCTAssertTrue(configuration.accepts(
            subprotocols: ["lish.raw-ethernet.v1", " \(capability)"],
            headers: [(name: "Origin", value: " \(origin) ")]
        ))
    }

    func testHandshakeRejectsWrongOrigin() throws {
        let configuration = try RawEthernetServerConfiguration(
            capability: capability,
            allowedOrigin: origin
        )
        XCTAssertFalse(configuration.accepts(
            subprotocols: ["lish.raw-ethernet.v1", capability],
            headers: [(name: "Origin", value: "http://127.0.0.1:4174")]
        ))
    }

    func testHandshakeRejectsDuplicateOriginHeaders() throws {
        let configuration = try RawEthernetServerConfiguration(
            capability: capability,
            allowedOrigin: origin
        )
        XCTAssertFalse(configuration.accepts(
            subprotocols: [RawEthernetServerConfiguration.protocolName, capability],
            headers: [
                (name: "Origin", value: origin),
                (name: "Origin", value: origin),
            ]
        ))
    }

    func testConfigurationRejectsOriginCredentials() {
        XCTAssertThrowsError(try RawEthernetServerConfiguration(
            capability: capability,
            allowedOrigin: "http://user@127.0.0.1:4173"
        ))
    }

    func testRejectsWrongCapability() throws {
        let configuration = try RawEthernetServerConfiguration(
            capability: capability,
            allowedOrigin: origin
        )
        XCTAssertFalse(configuration.accepts(
            subprotocols: [
                RawEthernetServerConfiguration.protocolName,
                String(repeating: "a", count: 64),
            ],
            headers: [(name: "Origin", value: origin)]
        ))
    }

    private func start(_ server: RawEthernetWebSocketServer) throws -> UInt16 {
        let started = expectation(description: "server started")
        let result = LockedResult<UInt16>()
        server.start {
            result.store($0)
            started.fulfill()
        }
        wait(for: [started], timeout: 3)
        return try XCTUnwrap(result.load()).get()
    }

    private func makeConnection(
        port: UInt16,
        capability: String,
        origin: String
    ) -> WebSocketTestConnection {
        var request = URLRequest(url: URL(string: "ws://127.0.0.1:\(port)/")!)
        request.setValue(
            "\(RawEthernetServerConfiguration.protocolName), \(capability)",
            forHTTPHeaderField: "Sec-WebSocket-Protocol"
        )
        request.setValue(origin, forHTTPHeaderField: "Origin")
        return WebSocketTestConnection(request: request)
    }

}

private func receiveOffer(
    _ task: URLSessionWebSocketTask,
    expectation: XCTestExpectation
) {
    task.receive { result in
        guard case .success(.data(let data)) = result else { return }
        if DHCPPacket.isOffer(data) {
            expectation.fulfill()
        } else {
            receiveOffer(task, expectation: expectation)
        }
    }
}

private final class WebSocketTestConnection: @unchecked Sendable {
    let task: URLSessionWebSocketTask
    private let delegate: WebSocketDelegate
    private let session: URLSession

    init(request: URLRequest) {
        delegate = WebSocketDelegate()
        session = URLSession(configuration: .ephemeral, delegate: delegate, delegateQueue: nil)
        task = session.webSocketTask(with: request)
    }

    func stop() {
        close(with: .goingAway)
        invalidate()
    }

    func onClose(_ handler: @escaping @Sendable (URLSessionWebSocketTask.CloseCode) -> Void) {
        delegate.onClose = handler
    }

    func close(with code: URLSessionWebSocketTask.CloseCode) {
        task.cancel(with: code, reason: nil)
    }

    func invalidate() {
        session.invalidateAndCancel()
    }
}

private final class WebSocketDelegate: NSObject, URLSessionWebSocketDelegate, @unchecked Sendable {
    var onClose: (@Sendable (URLSessionWebSocketTask.CloseCode) -> Void)?

    func urlSession(
        _ session: URLSession,
        webSocketTask: URLSessionWebSocketTask,
        didCloseWith closeCode: URLSessionWebSocketTask.CloseCode,
        reason: Data?
    ) {
        onClose?(closeCode)
    }
}

private final class LockedResult<Success>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: Result<Success, Error>?

    func store(_ value: Result<Success, Error>) {
        lock.lock()
        self.value = value
        lock.unlock()
    }

    func load() -> Result<Success, Error>? {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}
