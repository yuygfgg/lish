import Darwin
import Foundation

/// Ordered access to one native disk image.
///
/// Every operation runs on the same serial queue. A queued flush therefore
/// waits for all writes submitted before it, while ordinary writes do not
/// incur an fsync.
public final class DiskStore: @unchecked Sendable {
    public static let defaultMaximumOperationBytes = 64 * 1024

    public let url: URL
    public let geometry: DiskGeometry
    public let maximumOperationBytes: UInt64

    private let descriptor: Int32
    private let queue: DispatchQueue
    private let queueKey: DispatchSpecificKey<UInt8>
    private var closed = false

    public init(
        url: URL,
        maximumOperationBytes: Int = DiskStore.defaultMaximumOperationBytes,
        queue: DispatchQueue? = nil
    ) throws {
        guard maximumOperationBytes > 0 else {
            throw DiskError.operationTooLarge(length: 1, maximum: 0)
        }
        let descriptor = Darwin.open(url.path, O_RDWR | O_CLOEXEC)
        guard descriptor >= 0 else {
            throw DiskError.system(operation: "open", code: errno)
        }
        do {
            var info = stat()
            guard fstat(descriptor, &info) == 0 else {
                throw DiskError.system(operation: "fstat", code: errno)
            }
            guard (info.st_mode & S_IFMT) == S_IFREG else {
                throw DiskError.notRegular
            }
            guard info.st_size > 0 else {
                throw DiskError.emptyImage
            }
            let byteCount = UInt64(info.st_size)
            guard byteCount.isMultiple(of: DiskImage.sectorSize) else {
                throw DiskError.unalignedImage(byteCount: byteCount)
            }
            self.url = url.standardizedFileURL
            self.geometry = DiskGeometry(byteCount: byteCount)
            self.maximumOperationBytes = UInt64(maximumOperationBytes)
            let operationQueue = queue ?? DispatchQueue(label: "io.lish.disk-store")
            let queueKey = DispatchSpecificKey<UInt8>()
            operationQueue.setSpecific(key: queueKey, value: 1)
            self.descriptor = descriptor
            self.queue = operationQueue
            self.queueKey = queueKey
        } catch {
            Darwin.close(descriptor)
            throw error
        }
    }

    deinit {
        close()
    }

    /// Synchronous convenience for setup and tests. The actual I/O still uses
    /// the storage queue, so it preserves ordering with asynchronous requests.
    public func read(offset: UInt64, length: Int) throws -> Data {
        try queue.sync { try readLocked(offset: offset, length: length) }
    }

    public func write(offset: UInt64, data: Data) throws {
        let body = Data(data)
        try queue.sync { try writeLocked(offset: offset, data: body) }
    }

    public func flush() throws {
        try queue.sync { try flushLocked() }
    }

    public func readAsync(
        offset: UInt64,
        length: Int,
        completion: @escaping @Sendable (Result<Data, Error>) -> Void
    ) {
        queue.async { [self] in
            completion(Result { try readLocked(offset: offset, length: length) })
        }
    }

    public func writeAsync(
        offset: UInt64,
        data: Data,
        completion: @escaping @Sendable (Result<Void, Error>) -> Void
    ) {
        let body = Data(data)
        queue.async { [self] in
            completion(Result { try writeLocked(offset: offset, data: body) })
        }
    }

    public func flushAsync(
        completion: @escaping @Sendable (Result<Void, Error>) -> Void
    ) {
        queue.async { [self] in
            completion(Result { try flushLocked() })
        }
    }

    public func readAsync(offset: UInt64, length: Int) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            readAsync(offset: offset, length: length) { result in
                continuation.resume(with: result)
            }
        }
    }

    public func writeAsync(offset: UInt64, data: Data) async throws {
        try await withCheckedThrowingContinuation { continuation in
            writeAsync(offset: offset, data: data) { result in
                continuation.resume(with: result)
            }
        }
    }

    public func flushAsync() async throws {
        try await withCheckedThrowingContinuation { continuation in
            flushAsync { result in
                continuation.resume(with: result)
            }
        }
    }

    /// Close after all previously submitted operations. New operations fail.
    public func close() {
        let closeLocked = { [self] in
            guard !self.closed else { return }
            self.closed = true
            Darwin.close(self.descriptor)
        }
        if DispatchQueue.getSpecific(key: queueKey) != nil {
            closeLocked()
        } else {
            queue.sync(execute: closeLocked)
        }
    }

    private func checkedRange(offset: UInt64, length: UInt64, allowEmpty: Bool) throws {
        if !allowEmpty && length == 0 {
            throw DiskError.emptyRange
        }
        guard length <= maximumOperationBytes else {
            throw DiskError.operationTooLarge(length: length, maximum: maximumOperationBytes)
        }
        guard offset <= geometry.byteCount,
              length <= geometry.byteCount - offset
        else {
            throw DiskError.rangeOutOfBounds(
                offset: offset,
                length: length,
                byteCount: geometry.byteCount
            )
        }
        guard offset <= UInt64(Int64.max) else {
            throw DiskError.rangeOutOfBounds(
                offset: offset,
                length: length,
                byteCount: geometry.byteCount
            )
        }
    }

    private func readLocked(offset: UInt64, length: Int) throws -> Data {
        guard !closed else { throw DiskError.closed }
        guard length >= 0 else { throw DiskError.emptyRange }
        try checkedRange(offset: offset, length: UInt64(length), allowEmpty: false)

        var output = Data(count: length)
        var total = 0
        try output.withUnsafeMutableBytes { bytes in
            guard let base = bytes.baseAddress else { throw DiskError.emptyRange }
            while total < length {
                let result = Darwin.pread(
                    descriptor,
                    base.advanced(by: total),
                    length - total,
                    off_t(offset + UInt64(total))
                )
                if result < 0 {
                    if errno == EINTR { continue }
                    throw DiskError.system(operation: "pread", code: errno)
                }
                if result == 0 {
                    throw DiskError.shortRead(expected: UInt64(length), actual: UInt64(total))
                }
                total += result
            }
        }
        return output
    }

    private func writeLocked(offset: UInt64, data: Data) throws {
        guard !closed else { throw DiskError.closed }
        guard !data.isEmpty else { throw DiskError.emptyWrite }
        try checkedRange(offset: offset, length: UInt64(data.count), allowEmpty: true)

        var total = 0
        try data.withUnsafeBytes { bytes in
            guard let base = bytes.baseAddress else { throw DiskError.emptyWrite }
            while total < data.count {
                let result = Darwin.pwrite(
                    descriptor,
                    base.advanced(by: total),
                    data.count - total,
                    off_t(offset + UInt64(total))
                )
                if result < 0 {
                    if errno == EINTR { continue }
                    throw DiskError.system(operation: "pwrite", code: errno)
                }
                if result == 0 {
                    throw DiskError.shortWrite(expected: UInt64(data.count), actual: UInt64(total))
                }
                total += result
            }
        }
    }

    private func flushLocked() throws {
        guard !closed else { throw DiskError.closed }
        guard Darwin.fsync(descriptor) == 0 else {
            throw DiskError.system(operation: "fsync", code: errno)
        }
    }
}

/// Name used by the native HTTP/Worker host layer.
public typealias DiskService = DiskStore
