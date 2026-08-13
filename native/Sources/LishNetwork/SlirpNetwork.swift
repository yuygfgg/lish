import CLishSlirp
import Foundation

public enum SlirpNetworkError: Error, CustomStringConvertible {
    case createFailed(String)
    case invalidFrameLength(Int)
    case operationFailed(String)
    case stopped

    public var description: String {
        switch self {
        case .createFailed(let message): return message
        case .invalidFrameLength(let length): return "invalid Ethernet frame length: \(length)"
        case .operationFailed(let message): return message
        case .stopped: return "libslirp network is stopped"
        }
    }
}

public struct SlirpNetworkStatistics: Equatable, Sendable {
    public let framesFromGuest: UInt64
    public let bytesFromGuest: UInt64
    public let dropsFromGuest: UInt64
    public let framesToGuest: UInt64
    public let bytesToGuest: UInt64
    public let dropsToGuest: UInt64
    public let queuedFromGuest: UInt32
    public let queuedToGuest: UInt32
}

private final class OutputSignal: @unchecked Sendable {
    private let lock = NSLock()
    private let queue: DispatchQueue
    private var callback: (@Sendable () -> Void)?

    init(queue: DispatchQueue, callback: @escaping @Sendable () -> Void) {
        self.queue = queue
        self.callback = callback
    }

    func send() {
        queue.async { [weak self] in
            self?.invoke()
        }
    }

    func invalidate() {
        lock.lock()
        callback = nil
        lock.unlock()
    }

    private func invoke() {
        lock.lock()
        let current = callback
        lock.unlock()
        current?()
    }
}

private let outputReadyCallback: @convention(c) (UnsafeMutableRawPointer?) -> Void = { opaque in
    guard let opaque else { return }
    Unmanaged<OutputSignal>.fromOpaque(opaque).takeUnretainedValue().send()
}

public final class SlirpNetwork: @unchecked Sendable {
    public static let maximumFrameSize = 1_600

    private let lifecycle = NSCondition()
    private var handle: OpaquePointer?
    private var activeOperations = 0
    private let outputSignal: OutputSignal

    public init(
        queueCapacity: UInt32 = 256,
        disableHostLoopback: Bool = true,
        outputQueue: DispatchQueue,
        outputReady: @escaping @Sendable () -> Void
    ) throws {
        let signal = OutputSignal(queue: outputQueue, callback: outputReady)
        self.outputSignal = signal
        var config = lish_slirp_config_t(
            queue_capacity: queueCapacity,
            disable_host_loopback: disableHostLoopback
        )
        var error = [CChar](repeating: 0, count: 256)
        let opaque = Unmanaged.passUnretained(signal).toOpaque()
        let created = error.withUnsafeMutableBufferPointer { errorBuffer in
            lish_slirp_create(
                &config,
                outputReadyCallback,
                opaque,
                errorBuffer.baseAddress,
                errorBuffer.count
            )
        }
        guard let created else {
            let message = String(decoding: error.prefix { $0 != 0 }.map(UInt8.init), as: UTF8.self)
            throw SlirpNetworkError.createFailed(message)
        }
        handle = created
    }

    deinit {
        stop()
    }

    /// Returns false when the bounded guest-to-host queue is full.
    public func sendFromGuest(_ frame: Data) throws -> Bool {
        guard !frame.isEmpty, frame.count <= Self.maximumFrameSize else {
            throw SlirpNetworkError.invalidFrameLength(frame.count)
        }
        let result = try withHandle { current in
            frame.withUnsafeBytes { bytes in
                lish_slirp_input(
                    current,
                    bytes.bindMemory(to: UInt8.self).baseAddress,
                    bytes.count
                )
            }
        }
        if result < 0 { throw SlirpNetworkError.invalidFrameLength(frame.count) }
        return result == 1
    }

    public func nextFrameForGuest() throws -> Data? {
        var storage = [UInt8](repeating: 0, count: Self.maximumFrameSize)
        var length = 0
        let result = try withHandle { current in
            storage.withUnsafeMutableBufferPointer { buffer in
                lish_slirp_next_output(current, buffer.baseAddress, buffer.count, &length)
            }
        }
        if result == 0 { return nil }
        if result < 0 {
            throw SlirpNetworkError.operationFailed("libslirp returned an oversized Ethernet frame")
        }
        return Data(storage.prefix(length))
    }

    public func addHostForward(
        protocol transport: TransportProtocol = .tcp,
        hostAddress: String = "127.0.0.1",
        hostPort: UInt16,
        guestAddress: String = "10.0.2.15",
        guestPort: UInt16
    ) throws {
        let result = try withHandle { current in
            hostAddress.withCString { host in
                guestAddress.withCString { guest in
                    lish_slirp_add_host_forward(
                        current,
                        transport == .udp,
                        host,
                        hostPort,
                        guest,
                        guestPort
                    )
                }
            }
        }
        guard result == 0 else {
            throw SlirpNetworkError.operationFailed("unable to add the libslirp host forward")
        }
    }

    public func removeHostForward(
        protocol transport: TransportProtocol = .tcp,
        hostAddress: String = "127.0.0.1",
        hostPort: UInt16
    ) throws {
        let result = try withHandle { current in
            hostAddress.withCString { host in
                lish_slirp_remove_host_forward(current, transport == .udp, host, hostPort)
            }
        }
        guard result == 0 else {
            throw SlirpNetworkError.operationFailed("unable to remove the libslirp host forward")
        }
    }

    public func statistics() throws -> SlirpNetworkStatistics {
        let value = try withHandle { current in
            var value = lish_slirp_stats_t()
            lish_slirp_get_stats(current, &value)
            return value
        }
        return SlirpNetworkStatistics(
            framesFromGuest: value.frames_from_guest,
            bytesFromGuest: value.bytes_from_guest,
            dropsFromGuest: value.drops_from_guest,
            framesToGuest: value.frames_to_guest,
            bytesToGuest: value.bytes_to_guest,
            dropsToGuest: value.drops_to_guest,
            queuedFromGuest: value.queued_from_guest,
            queuedToGuest: value.queued_to_guest
        )
    }

    public func stop() {
        outputSignal.invalidate()
        lifecycle.lock()
        let current = handle
        handle = nil
        lifecycle.unlock()
        guard let current else { return }

        lish_slirp_stop(current)
        lifecycle.lock()
        while activeOperations != 0 {
            lifecycle.wait()
        }
        lifecycle.unlock()
        lish_slirp_destroy(current)
    }

    private func withHandle<T>(_ body: (OpaquePointer) throws -> T) throws -> T {
        lifecycle.lock()
        guard let current = handle else {
            lifecycle.unlock()
            throw SlirpNetworkError.stopped
        }
        activeOperations += 1
        lifecycle.unlock()
        defer {
            lifecycle.lock()
            activeOperations -= 1
            if activeOperations == 0 {
                lifecycle.broadcast()
            }
            lifecycle.unlock()
        }
        return try body(current)
    }
}

public enum TransportProtocol: Sendable {
    case tcp
    case udp
}
