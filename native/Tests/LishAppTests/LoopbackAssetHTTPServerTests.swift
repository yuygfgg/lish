import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import XCTest
@testable import LishApp

final class LoopbackAssetHTTPServerTests: XCTestCase {
    private let capability = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("lish-assets-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        if let directory {
            try? FileManager.default.removeItem(at: directory)
        }
    }

    func testLargeAssetResponseIsComplete() throws {
        let asset = Data((0..<(4 * 1024 * 1024)).map { UInt8(truncatingIfNeeded: $0) })
        try asset.write(to: directory.appendingPathComponent("kernel.bin"))
        let configuration = try AssetHTTPServerConfiguration(
            rootURL: directory,
            capability: capability
        )
        let server = LoopbackAssetHTTPServer(configuration: configuration)
        defer { server.stop() }

        let started = expectation(description: "asset server started")
        let portResult = LockedAssetResult<UInt16>()
        server.start {
            portResult.store($0)
            started.fulfill()
        }
        wait(for: [started], timeout: 3)
        let port = try XCTUnwrap(portResult.load()).get()
        let url = configuration.pageURL(port: port, path: "kernel.bin")

        let completed = expectation(description: "asset response")
        let responseResult = LockedAssetResult<(HTTPURLResponse, Data)>()
        let session = URLSession(configuration: .ephemeral)
        session.dataTask(with: url) { data, response, error in
            responseResult.store(Result {
                if let error { throw error }
                guard let response = response as? HTTPURLResponse else {
                    throw AssetHTTPTestError.invalidResponse
                }
                return (response, data ?? Data())
            })
            completed.fulfill()
        }.resume()
        wait(for: [completed], timeout: 5)
        session.finishTasksAndInvalidate()

        let (response, body) = try XCTUnwrap(responseResult.load()).get()
        XCTAssertEqual(response.statusCode, 200)
        XCTAssertEqual(response.value(forHTTPHeaderField: "Content-Length"), String(asset.count))
        XCTAssertEqual(body, asset)
    }
}

private enum AssetHTTPTestError: Error {
    case invalidResponse
}

private final class LockedAssetResult<Success>: @unchecked Sendable {
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
