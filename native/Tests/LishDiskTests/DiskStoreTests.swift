import Foundation
import XCTest
@testable import LishDisk

final class DiskStoreTests: XCTestCase {
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("lish-disk-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        if let directory {
            try? FileManager.default.removeItem(at: directory)
        }
    }

    func testImageValidationRequiresRegularNonEmptyAlignedFile() throws {
        let valid = directory.appendingPathComponent("valid.img")
        try Data(repeating: 0x11, count: 1024).write(to: valid)
        XCTAssertEqual(try DiskImage.inspect(valid), DiskGeometry(byteCount: 1024))

        let empty = directory.appendingPathComponent("empty.img")
        FileManager.default.createFile(atPath: empty.path, contents: Data())
        XCTAssertThrowsError(try DiskImage.inspect(empty)) { error in
            XCTAssertEqual(error as? DiskError, .emptyImage)
        }

        let unaligned = directory.appendingPathComponent("unaligned.img")
        try Data(repeating: 0, count: 513).write(to: unaligned)
        XCTAssertThrowsError(try DiskImage.inspect(unaligned)) { error in
            guard case .unalignedImage(byteCount: 513) = error as? DiskError else {
                return XCTFail("unexpected error: \(error)")
            }
        }

        XCTAssertThrowsError(try DiskImage.inspect(directory)) { error in
            XCTAssertEqual(error as? DiskError, .notRegular)
        }
    }

    func testCloneOrCopyPublishesIndependentWritableImage() throws {
        let source = directory.appendingPathComponent("base.img")
        let destination = directory.appendingPathComponent("vm/disk.img")
        try Data(repeating: 0x2a, count: 1024).write(to: source)

        XCTAssertEqual(
            try DiskImage.createWritableImage(from: source, at: destination),
            DiskGeometry(byteCount: 1024)
        )
        var bytes = try Data(contentsOf: destination)
        XCTAssertEqual(bytes, Data(repeating: 0x2a, count: 1024))
        bytes[0] = 0x7f
        try bytes.write(to: destination)
        XCTAssertEqual(try Data(contentsOf: source)[0], 0x2a)
    }

    func testRangeAndEmptyWriteValidationDoesNotModifyImage() throws {
        let image = directory.appendingPathComponent("disk.img")
        try Data(repeating: 0x33, count: 1024).write(to: image)
        let store = try DiskStore(url: image)

        XCTAssertThrowsError(try store.read(offset: 1024, length: 1))
        XCTAssertThrowsError(try store.read(offset: 1023, length: 2))
        XCTAssertThrowsError(try store.write(offset: 0, data: Data())) { error in
            XCTAssertEqual(error as? DiskError, .emptyWrite)
        }
        XCTAssertThrowsError(try store.write(offset: 1024, data: Data([1])))
        XCTAssertEqual(try store.read(offset: 0, length: 1024), Data(repeating: 0x33, count: 1024))
    }

    func testSerialWritesRetainSubmissionOrderAndFlushes() throws {
        let image = directory.appendingPathComponent("disk.img")
        try Data(repeating: 0, count: 2048).write(to: image)
        let store = try DiskStore(url: image)
        let expectation = expectation(description: "ordered operations")
        expectation.expectedFulfillmentCount = 3
        let completions = LockedStrings()

        store.writeAsync(offset: 0, data: Data(repeating: 0xa1, count: 512)) { result in
            if case .failure(let error) = result {
                XCTFail("first write failed: \(error)")
            }
            completions.append("first")
            expectation.fulfill()
        }
        store.writeAsync(offset: 512, data: Data(repeating: 0xb2, count: 512)) { result in
            if case .failure(let error) = result {
                XCTFail("second write failed: \(error)")
            }
            completions.append("second")
            expectation.fulfill()
        }
        store.flushAsync { result in
            if case .failure(let error) = result {
                XCTFail("flush failed: \(error)")
            }
            completions.append("flush")
            expectation.fulfill()
        }
        wait(for: [expectation], timeout: 3)
        XCTAssertEqual(completions.values, ["first", "second", "flush"])
        XCTAssertEqual(try store.read(offset: 0, length: 1024), Data(repeating: 0xa1, count: 512) + Data(repeating: 0xb2, count: 512))
    }

    func testRetryingTheSameWriteIsIdempotent() throws {
        let image = directory.appendingPathComponent("disk.img")
        try Data(repeating: 0, count: 1024).write(to: image)
        let store = try DiskStore(url: image)
        let body = Data(repeating: 0xc3, count: 512)
        try store.write(offset: 0, data: body)
        try store.write(offset: 0, data: body)
        try store.flush()
        XCTAssertEqual(try Data(contentsOf: image), body + Data(repeating: 0, count: 512))
    }
}

private final class LockedStrings: @unchecked Sendable {
    private let lock = NSLock()
    private var storage: [String] = []

    func append(_ value: String) {
        lock.lock()
        storage.append(value)
        lock.unlock()
    }

    var values: [String] {
        lock.lock()
        defer { lock.unlock() }
        return storage
    }
}
