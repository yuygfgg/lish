import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import XCTest
@testable import LishDisk

final class DiskHTTPServerTests: XCTestCase {
    private let capability = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    private let vmID = "test-vm"
    private let origin = "http://127.0.0.1:4173"
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("lish-disk-http-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        if let directory {
            try? FileManager.default.removeItem(at: directory)
        }
    }

    func testAuthenticatedHeadReadWriteAndFlush() throws {
        let (server, store, port) = try makeServer()
        defer {
            server.stop()
            store.close()
        }
        let path = try configuration().diskPath

        let head = try send(
            port: port,
            request: request(method: "HEAD", target: path)
        )
        XCTAssertEqual(head.status, 200)
        XCTAssertEqual(head.headers["x-lish-disk-size"], "1024")
        XCTAssertEqual(head.headers["access-control-expose-headers"], "X-Lish-Disk-Size")
        XCTAssertEqual(head.headers["access-control-allow-origin"], origin)
        XCTAssertEqual(head.headers["cache-control"], "no-store")
        XCTAssertTrue(head.body.isEmpty)

        let initial = try send(
            port: port,
            request: request(method: "GET", target: "\(path)?offset=4&length=4")
        )
        XCTAssertEqual(initial.status, 200)
        XCTAssertEqual(initial.body, Data(repeating: 0x11, count: 4))

        let body = Data([0xa1, 0xb2, 0xc3, 0xd4])
        let write = try send(
            port: port,
            request: request(
                method: "PUT",
                target: "\(path)?offset=4",
                body: body,
                headers: ["Content-Type": "application/octet-stream"]
            )
        )
        XCTAssertEqual(write.status, 204)
        XCTAssertTrue(write.body.isEmpty)

        let flush = try send(
            port: port,
            request: request(method: "POST", target: "\(path)/flush")
        )
        XCTAssertEqual(flush.status, 204)
        XCTAssertEqual(try Data(contentsOf: store.url)[4..<8], body[...])
    }

    func testMaximumReadReturnsTheCompleteResponseBody() throws {
        let image = Data((0..<(128 * 1024)).map { UInt8(truncatingIfNeeded: $0) })
        let (server, store, port) = try makeServer(image: image)
        defer {
            server.stop()
            store.close()
        }
        let path = try configuration().diskPath
        let offset = 64 * 1024
        let length = DiskStore.defaultMaximumOperationBytes

        let response = try send(
            port: port,
            request: request(
                method: "GET",
                target: "\(path)?offset=\(offset)&length=\(length)"
            )
        )

        XCTAssertEqual(response.status, 200)
        XCTAssertEqual(response.headers["content-length"], String(length))
        XCTAssertEqual(response.body, image[offset..<(offset + length)])
    }

    func testCorsPreflightAllowsDiskWriteWithOffset() throws {
        let (server, store, port) = try makeServer()
        defer {
            server.stop()
            store.close()
        }
        let path = try configuration().diskPath
        let response = try send(
            port: port,
            request: request(
                method: "OPTIONS",
                target: "\(path)?offset=4",
                headers: [
                    "Access-Control-Request-Method": "PUT",
                    "Access-Control-Request-Headers": "content-type",
                ]
            )
        )

        XCTAssertEqual(response.status, 204)
        XCTAssertEqual(response.headers["access-control-allow-origin"], origin)
        XCTAssertEqual(
            response.headers["access-control-allow-methods"],
            "GET, HEAD, PUT, POST, OPTIONS"
        )
        XCTAssertEqual(response.headers["access-control-allow-headers"], "content-type")

        XCTAssertEqual(try send(
            port: port,
            request: request(
                method: "OPTIONS",
                target: "\(path)?offset=4",
                headers: ["Access-Control-Request-Method": "PUT"]
            )
        ).status, 204)
    }

    func testCorsPreflightRejectsInvalidDiskRequests() throws {
        let (server, store, port) = try makeServer()
        defer {
            server.stop()
            store.close()
        }
        let path = try configuration().diskPath

        XCTAssertEqual(try send(
            port: port,
            request: request(method: "OPTIONS", target: "\(path)?offset=4")
        ).status, 400)
        XCTAssertEqual(try send(
            port: port,
            request: request(
                method: "OPTIONS",
                target: path,
                headers: ["Access-Control-Request-Method": "PUT"]
            )
        ).status, 400)
        XCTAssertEqual(try send(
            port: port,
            request: request(
                method: "OPTIONS",
                target: "\(path)?offset=4",
                headers: [
                    "Access-Control-Request-Method": "PUT",
                    "Access-Control-Request-Headers": "authorization",
                ]
            )
        ).status, 400)
    }

    func testRequestValidationAndRangeErrors() throws {
        let (server, store, port) = try makeServer()
        defer {
            server.stop()
            store.close()
        }
        let path = try configuration().diskPath

        XCTAssertEqual(try send(
            port: port,
            request: request(method: "GET", target: "\(path)?offset=1024&length=1")
        ).status, 416)
        XCTAssertEqual(try send(
            port: port,
            request: request(method: "PUT", target: "\(path)?offset=1024", body: Data([1]))
        ).status, 416)
        XCTAssertEqual(try send(
            port: port,
            request: request(method: "HEAD", target: "\(path)?offset=0")
        ).status, 400)
        XCTAssertEqual(try send(
            port: port,
            request: request(method: "GET", target: "\(path)?offset=0&length=1&extra=1")
        ).status, 400)
        XCTAssertEqual(try send(
            port: port,
            request: request(method: "PUT", target: "\(path)?offset=0")
        ).status, 400)
        XCTAssertEqual(try send(
            port: port,
            request: request(method: "POST", target: "\(path)/flush?extra=1")
        ).status, 405)
        XCTAssertEqual(try send(
            port: port,
            request: request(
                method: "GET",
                target: "\(path)?offset=0&length=1",
                origin: "http://127.0.0.1:9999"
            )
        ).status, 403)
        XCTAssertEqual(try send(
            port: port,
            request: request(method: "GET", target: "/s/wrong/vms/\(vmID)/disk?offset=0&length=1")
        ).status, 404)
    }

    func testClosedStoreFailuresAreInternalErrors() throws {
        let (server, store, port) = try makeServer()
        defer { server.stop() }
        let path = try configuration().diskPath
        store.close()

        XCTAssertEqual(try send(
            port: port,
            request: request(method: "GET", target: "\(path)?offset=0&length=1")
        ).status, 500)
        XCTAssertEqual(try send(
            port: port,
            request: request(method: "PUT", target: "\(path)?offset=0", body: Data([1]))
        ).status, 500)
        XCTAssertEqual(try send(
            port: port,
            request: request(method: "POST", target: "\(path)/flush")
        ).status, 500)
    }

    private func configuration() throws -> DiskHTTPServerConfiguration {
        try DiskHTTPServerConfiguration(
            capability: capability,
            vmID: vmID,
            allowedOrigin: origin
        )
    }

    private func makeServer(
        image data: Data = Data(repeating: 0x11, count: 1024)
    ) throws -> (DiskHTTPServer, DiskStore, UInt16) {
        let image = directory.appendingPathComponent("disk.img")
        try data.write(to: image)
        let store = try DiskStore(url: image)
        let server = DiskHTTPServer(store: store, configuration: try configuration())
        let started = expectation(description: "disk HTTP server started")
        let result = LockedHTTPResult<UInt16>()
        server.start {
            result.store($0)
            started.fulfill()
        }
        wait(for: [started], timeout: 3)
        return (server, store, try XCTUnwrap(result.load()).get())
    }

    private func request(
        method: String,
        target: String,
        body: Data = Data(),
        origin requestOrigin: String? = nil,
        headers: [String: String] = [:]
    ) -> HTTPTestRequest {
        HTTPTestRequest(
            method: method,
            target: target,
            body: body,
            origin: requestOrigin ?? origin,
            headers: headers
        )
    }

    private func send(port: UInt16, request: HTTPTestRequest) throws -> HTTPTestResponse {
        guard let url = URL(string: "http://127.0.0.1:\(port)\(request.target)") else {
            throw HTTPTestError.invalidResponse
        }
        var urlRequest = URLRequest(url: url)
        urlRequest.httpMethod = request.method
        urlRequest.httpBody = request.method == "PUT" || !request.body.isEmpty
            ? request.body
            : nil
        urlRequest.setValue(request.origin, forHTTPHeaderField: "Origin")
        for (name, value) in request.headers {
            urlRequest.setValue(value, forHTTPHeaderField: name)
        }

        let completed = expectation(description: "HTTP response")
        let result = LockedHTTPResult<HTTPTestResponse>()
        let session = URLSession(configuration: .ephemeral)
        session.dataTask(with: urlRequest) { data, response, error in
            result.store(Result {
                if let error { throw error }
                guard let response = response as? HTTPURLResponse else {
                    throw HTTPTestError.invalidResponse
                }
                return HTTPTestResponse(response: response, body: data ?? Data())
            })
            completed.fulfill()
        }.resume()
        wait(for: [completed], timeout: 3)
        session.finishTasksAndInvalidate()
        return try XCTUnwrap(result.load()).get()
    }
}

private struct HTTPTestRequest {
    let method: String
    let target: String
    let body: Data
    let origin: String
    let headers: [String: String]
}

private struct HTTPTestResponse {
    let status: Int
    let headers: [String: String]
    let body: Data

    init(response: HTTPURLResponse, body: Data) {
        var headers: [String: String] = [:]
        for (name, value) in response.allHeaderFields {
            headers[String(describing: name).lowercased()] = String(describing: value)
        }
        self.status = response.statusCode
        self.headers = headers
        self.body = body
    }
}

private enum HTTPTestError: Error {
    case invalidResponse
}

private final class LockedHTTPResult<Success>: @unchecked Sendable {
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
