import Darwin
import Foundation

/// The fixed sector size used by the virtio-blk protocol.
public enum DiskImage {
    public static let sectorSize: UInt64 = 512

    /// Return the geometry of a regular, non-empty, sector-aligned raw image.
    ///
    /// The image is opened read-only. This check does not allocate memory or
    /// read the image body.
    public static func inspect(_ url: URL) throws -> DiskGeometry {
        let descriptor = Darwin.open(url.path, O_RDONLY | O_CLOEXEC)
        guard descriptor >= 0 else {
            throw DiskError.system(operation: "open", code: errno)
        }
        defer { Darwin.close(descriptor) }

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
        guard byteCount.isMultiple(of: sectorSize) else {
            throw DiskError.unalignedImage(byteCount: byteCount)
        }
        return DiskGeometry(byteCount: byteCount)
    }

    /// Validate an image. This spelling is convenient at application setup.
    public static func validate(_ url: URL) throws -> DiskGeometry {
        try inspect(url)
    }

    /// Create a writable image beside the destination and publish it with one
    /// rename only after the complete clone or copy succeeds.
    @discardableResult
    public static func createWritableImage(
        from sourceURL: URL,
        at destinationURL: URL
    ) throws -> DiskGeometry {
        let source = sourceURL.standardizedFileURL
        let destination = destinationURL.standardizedFileURL
        guard source != destination else {
            throw DiskError.sameImage
        }

        let geometry = try inspect(source)
        let fileManager = FileManager.default
        let parent = destination.deletingLastPathComponent()
        try fileManager.createDirectory(at: parent, withIntermediateDirectories: true)

        let temporary = parent.appendingPathComponent(
            ".\(destination.lastPathComponent).\(UUID().uuidString).tmp"
        )
        var published = false
        defer {
            if !published {
                try? fileManager.removeItem(at: temporary)
            }
        }

        // APFS clonefile is constant-time and keeps the base image immutable.
        // Some filesystems do not support it, so a normal copy is the required
        // portable fallback.
        if clonefile(source.path, temporary.path, 0) != 0 {
            try fileManager.copyItem(at: source, to: temporary)
        }

        let temporaryGeometry = try inspect(temporary)
        guard temporaryGeometry == geometry else {
            throw DiskError.copyChangedSource
        }
        try fileManager.moveItem(at: temporary, to: destination)
        published = true
        return geometry
    }

    /// Short alias for callers that do not need the longer setup spelling.
    @discardableResult
    public static func cloneOrCopy(
        from sourceURL: URL,
        to destinationURL: URL
    ) throws -> DiskGeometry {
        try createWritableImage(from: sourceURL, at: destinationURL)
    }
}

public struct DiskGeometry: Equatable, Sendable {
    public let byteCount: UInt64
    public let sectorCount: UInt64

    public init(byteCount: UInt64) {
        self.byteCount = byteCount
        self.sectorCount = byteCount / DiskImage.sectorSize
    }
}

public enum DiskError: Error, Equatable, Sendable, CustomStringConvertible {
    case notRegular
    case emptyImage
    case unalignedImage(byteCount: UInt64)
    case sameImage
    case copyChangedSource
    case closed
    case emptyRange
    case emptyWrite
    case rangeOutOfBounds(offset: UInt64, length: UInt64, byteCount: UInt64)
    case operationTooLarge(length: UInt64, maximum: UInt64)
    case shortRead(expected: UInt64, actual: UInt64)
    case shortWrite(expected: UInt64, actual: UInt64)
    case system(operation: String, code: Int32)

    public var description: String {
        switch self {
        case .notRegular: return "disk image is not a regular file"
        case .emptyImage: return "disk image is empty"
        case .unalignedImage(let byteCount):
            return "disk image size is not a multiple of 512 bytes: \(byteCount)"
        case .sameImage: return "source and destination disk images are the same file"
        case .copyChangedSource: return "disk image changed while it was copied"
        case .closed: return "disk store is closed"
        case .emptyRange: return "disk read range must not be empty"
        case .emptyWrite: return "disk write body must not be empty"
        case .rangeOutOfBounds(let offset, let length, let byteCount):
            return "disk range is outside the image: offset=\(offset) length=\(length) size=\(byteCount)"
        case .operationTooLarge(let length, let maximum):
            return "disk operation is too large: \(length) > \(maximum) bytes"
        case .shortRead(let expected, let actual):
            return "disk read was short: expected \(expected), received \(actual)"
        case .shortWrite(let expected, let actual):
            return "disk write was short: expected \(expected), wrote \(actual)"
        case .system(let operation, let code):
            return "disk \(operation) failed (errno \(code))"
        }
    }
}

// Keep the domain-specific names available to hosts that prefer them.
public typealias DiskImageError = DiskError
public typealias DiskStoreError = DiskError
