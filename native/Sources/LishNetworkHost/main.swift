import Dispatch
import Foundation
import LishNetwork

struct Arguments {
    var origin = "http://127.0.0.1:4173"
    var capability: String?
    var port: UInt16 = 0
    var allowRemoteConnections = false

    init() throws {
        var values = CommandLine.arguments.dropFirst().makeIterator()
        while let argument = values.next() {
            switch argument {
            case "--origin":
                guard let value = values.next() else { throw UsageError() }
                origin = value
            case "--capability":
                guard let value = values.next() else { throw UsageError() }
                capability = value
            case "--port":
                guard let value = values.next(), let parsed = UInt16(value) else { throw UsageError() }
                port = parsed
            case "--allow-remote":
                allowRemoteConnections = true
            default:
                throw UsageError()
            }
        }
    }
}

struct UsageError: Error {}

final class StartResult: @unchecked Sendable {
    private let lock = NSLock()
    private var value: Result<UInt16, Error>?

    func store(_ result: Result<UInt16, Error>) {
        lock.lock()
        value = result
        lock.unlock()
    }

    func load() -> Result<UInt16, Error>? {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

func randomCapability() -> String {
    UUID().uuidString.replacingOccurrences(of: "-", with: "") +
        UUID().uuidString.replacingOccurrences(of: "-", with: "")
}

do {
    let arguments = try Arguments()
    let capability = arguments.capability ?? randomCapability()
    let configuration = try RawEthernetServerConfiguration(
        capability: capability,
        allowedOrigin: arguments.origin,
        port: arguments.port,
        allowRemoteConnections: arguments.allowRemoteConnections
    )
    let server = RawEthernetWebSocketServer(configuration: configuration)
    let start = DispatchSemaphore(value: 0)
    let startResult = StartResult()
    server.start { result in
        startResult.store(result)
        start.signal()
    }
    start.wait()
    guard let result = startResult.load() else {
        throw RawEthernetServerError.listenerFailed("network host did not report startup status")
    }
    let port = try result.get()
    guard let origin = URL(string: arguments.origin) else {
        throw RawEthernetServerError.invalidConfiguration("origin is not a URL")
    }
    let advertisedHost = origin.host ?? "127.0.0.1"
    print("LISTENING ws://\(advertisedHost):\(port)/")
    print("CAPABILITY \(capability)")
    print("ORIGIN \(arguments.origin)")
    print("Open the Lish page with:")
    print("?network=ws://\(advertisedHost):\(port)/&capability=\(capability)")
    fflush(stdout)
    withExtendedLifetime(server) { dispatchMain() }
} catch is UsageError {
    fputs(
        "usage: lish-network-host [--origin URL] [--capability TOKEN] [--port PORT] [--allow-remote]\n",
        stderr
    )
    exit(2)
} catch {
    fputs("lish-network-host: \(error)\n", stderr)
    exit(1)
}
