import Darwin
import Foundation
import XCTest
@testable import LishNetwork

final class SlirpNetworkTests: XCTestCase {
    func testDHCPDiscoverReceivesOffer() throws {
        let ready = expectation(description: "libslirp produced a frame")
        let network = try SlirpNetwork(outputQueue: .global(), outputReady: { ready.fulfill() })
        defer { network.stop() }

        XCTAssertTrue(try network.sendFromGuest(DHCPPacket.discover()))
        wait(for: [ready], timeout: 2)

        var offer: Data?
        let deadline = Date().addingTimeInterval(1)
        repeat {
            if let frame = try network.nextFrameForGuest(), DHCPPacket.isOffer(frame) {
                offer = frame
                break
            }
            usleep(1_000)
        } while Date() < deadline

        XCTAssertNotNil(offer)
        let statistics = try network.statistics()
        XCTAssertEqual(statistics.framesFromGuest, 1)
        XCTAssertGreaterThanOrEqual(statistics.framesToGuest, 1)
        XCTAssertEqual(statistics.dropsFromGuest, 0)
        XCTAssertEqual(statistics.dropsToGuest, 0)
    }

    func testUDPDatagramReceivesResponse() throws {
        let echo = try UDPEchoServer(datagramCount: 1)
        defer { echo.stop() }
        let network = try SlirpNetwork(
            disableHostLoopback: false,
            outputQueue: .global(),
            outputReady: {}
        )
        defer { network.stop() }

        XCTAssertTrue(try network.sendFromGuest(ARPPacket.request()))
        let arpDeadline = Date().addingTimeInterval(1)
        var arpReply = false
        repeat {
            if let frame = try network.nextFrameForGuest(), ARPPacket.isReply(frame) {
                arpReply = true
                break
            }
            usleep(1_000)
        } while Date() < arpDeadline
        XCTAssertTrue(arpReply)

        XCTAssertTrue(try network.sendFromGuest(UDPPacket.datagram(destinationPort: echo.port)))
        var response: Data?
        var observedFrames: [String] = []
        let deadline = Date().addingTimeInterval(3)
        repeat {
            if let frame = try network.nextFrameForGuest() {
                observedFrames.append(UDPPacket.summary(frame))
                if UDPPacket.isResponse(frame, sourcePort: echo.port) {
                    response = frame
                    break
                }
            }
            usleep(1_000)
        } while Date() < deadline

        XCTAssertTrue(echo.received, "libslirp did not deliver the guest datagram to loopback")
        XCTAssertNotNil(
            response,
            "libslirp did not return the echoed datagram to the guest; frames=\(observedFrames)"
        )
        let statistics = try network.statistics()
        XCTAssertEqual(statistics.framesFromGuest, 2)
        XCTAssertGreaterThanOrEqual(statistics.framesToGuest, 2)
        XCTAssertEqual(statistics.dropsFromGuest, 0)
        XCTAssertEqual(statistics.dropsToGuest, 0)
    }

    func testOutputQueueAppliesBackpressureWithoutDroppingFrames() throws {
        let echo = try UDPEchoServer(datagramCount: 1, responseCopies: 2)
        defer { echo.stop() }
        let network = try SlirpNetwork(
            queueCapacity: 1,
            disableHostLoopback: false,
            outputQueue: .global(),
            outputReady: {}
        )
        defer { network.stop() }

        XCTAssertTrue(try network.sendFromGuest(ARPPacket.request()))
        XCTAssertNotNil(try waitForFrame(from: network, matching: ARPPacket.isReply))

        XCTAssertTrue(try network.sendFromGuest(UDPPacket.datagram(destinationPort: echo.port)))
        XCTAssertTrue(waitUntil { echo.receivedCount == 1 })
        XCTAssertTrue(waitUntil { (try? network.statistics().queuedToGuest) == 1 })

        XCTAssertNotNil(try waitForFrame(from: network) {
            UDPPacket.isResponse($0, sourcePort: echo.port)
        })
        XCTAssertNotNil(try waitForFrame(from: network) {
            UDPPacket.isResponse($0, sourcePort: echo.port)
        })

        let statistics = try network.statistics()
        XCTAssertEqual(statistics.dropsToGuest, 0)
        XCTAssertEqual(statistics.queuedToGuest, 0)
    }

    func testRejectsInvalidFrameSizes() throws {
        let network = try SlirpNetwork(outputQueue: .global(), outputReady: {})
        defer { network.stop() }
        XCTAssertThrowsError(try network.sendFromGuest(Data()))
        XCTAssertThrowsError(
            try network.sendFromGuest(Data(repeating: 0, count: SlirpNetwork.maximumFrameSize + 1))
        )
    }

    func testStopIsIdempotent() throws {
        let network = try SlirpNetwork(outputQueue: .global(), outputReady: {})
        network.stop()
        network.stop()
        XCTAssertThrowsError(try network.sendFromGuest(DHCPPacket.discover()))
    }

    func testStopUnblocksAFullOutputQueue() throws {
        let echo = try UDPEchoServer(datagramCount: 1, responseCopies: 2)
        defer { echo.stop() }
        let network = try SlirpNetwork(
            queueCapacity: 1,
            disableHostLoopback: false,
            outputQueue: .global(),
            outputReady: {}
        )

        XCTAssertTrue(try network.sendFromGuest(ARPPacket.request()))
        XCTAssertNotNil(try waitForFrame(from: network, matching: ARPPacket.isReply))
        XCTAssertTrue(try network.sendFromGuest(UDPPacket.datagram(destinationPort: echo.port)))
        XCTAssertTrue(waitUntil { echo.receivedCount == 1 })
        XCTAssertTrue(waitUntil { (try? network.statistics().queuedToGuest) == 1 })

        let stopped = expectation(description: "network stopped")
        DispatchQueue.global().async {
            network.stop()
            stopped.fulfill()
        }
        wait(for: [stopped], timeout: 2)
    }

    private func waitForFrame(
        from network: SlirpNetwork,
        matching predicate: (Data) -> Bool
    ) throws -> Data? {
        let deadline = Date().addingTimeInterval(2)
        repeat {
            if let frame = try network.nextFrameForGuest(), predicate(frame) {
                return frame
            }
            usleep(1_000)
        } while Date() < deadline
        return nil
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

private final class UDPEchoServer: @unchecked Sendable {
    let port: UInt16
    private let descriptor: Int32
    private let queue = DispatchQueue(label: "io.lish.network.udp-echo")
    private let lock = NSLock()
    private var stopped = false
    private let datagramCount: Int
    private let responseCopies: Int
    private var receivedDatagrams = 0

    var receivedCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return receivedDatagrams
    }

    var received: Bool { receivedCount != 0 }

    init(datagramCount: Int, responseCopies: Int = 1) throws {
        self.datagramCount = datagramCount
        self.responseCopies = responseCopies
        let descriptor = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
        guard descriptor >= 0 else { throw POSIXError(.ENFILE) }
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
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EINVAL)
        }

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
        guard bindResult == 0 else {
            close(descriptor)
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EINVAL)
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
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EINVAL)
        }
        port = UInt16(bigEndian: boundAddress.sin_port)

        queue.async { [weak self] in self?.echoDatagrams() }
    }

    func stop() {
        lock.lock()
        guard !stopped else {
            lock.unlock()
            return
        }
        stopped = true
        lock.unlock()
        close(descriptor)
    }

    private func echoDatagrams() {
        for _ in 0..<datagramCount {
            var bytes = [UInt8](repeating: 0, count: SlirpNetwork.maximumFrameSize)
            var peer = sockaddr_storage()
            var peerLength = socklen_t(MemoryLayout<sockaddr_storage>.size)
            let count = withUnsafeMutablePointer(to: &peer) { pointer in
                pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { address in
                    recvfrom(descriptor, &bytes, bytes.count, 0, address, &peerLength)
                }
            }
            guard count > 0 else { return }
            lock.lock()
            receivedDatagrams += 1
            lock.unlock()
            for _ in 0..<responseCopies {
                bytes.withUnsafeBytes { buffer in
                    withUnsafePointer(to: &peer) { pointer in
                        pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { address in
                            _ = sendto(
                                descriptor,
                                buffer.baseAddress,
                                count,
                                0,
                                address,
                                peerLength
                            )
                        }
                    }
                }
            }
        }
    }
}
