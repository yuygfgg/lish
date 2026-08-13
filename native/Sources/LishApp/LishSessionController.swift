import Foundation
import LishDisk
import LishNetwork

public enum LishSessionState: String, Sendable {
    case idle
    case preparing
    case loading
    case running
    case quiescing
    case suspended
    case failed
    case destroyed
}

public enum LishSessionError: Error, CustomStringConvertible, Sendable {
    case invalidState(LishSessionState, operation: String)
    case missingAsset(String)
    case listenerFailed(String)
    case noApplicationSupport

    public var description: String {
        switch self {
        case .invalidState(let state, let operation):
            return "cannot \(operation) while session is \(state.rawValue)"
        case .missingAsset(let path): return "required application asset is missing: \(path)"
        case .listenerFailed(let message): return "session listener failed: \(message)"
        case .noApplicationSupport: return "Application Support directory is unavailable"
        }
    }
}

/// Values passed to the page when a session's loopback services are ready.
public struct LishWebConfiguration: Codable, Equatable, Sendable {
    public let pageURL: URL
    public let origin: URL
    public let capability: String
    public let vmID: String
    public let diskURL: URL
    public let networkURL: URL
    public let networkProtocols: [String]
}

/// Owns one VM's disk and loopback services.
///
/// The controller is deliberately independent from WebKit. The AppKit shell
/// can recreate a page or a Worker without creating a second disk or network
/// context, and tests can exercise startup without a window.
@MainActor
public final class LishSessionController: NSObject {
    public private(set) var state: LishSessionState = .idle
    public private(set) var webConfiguration: LishWebConfiguration?
    public let capability: String
    public let vmID: String

    private let fileManager: FileManager
    private let assetRootURL: URL
    private let dataRootURL: URL
    private let baseDiskURL: URL
    private var assetServer: LoopbackAssetHTTPServer?
    private var diskServer: DiskHTTPServer?
    private var networkServer: RawEthernetWebSocketServer?
    private var diskStore: DiskStore?

    public init(
        assetRootURL: URL? = nil,
        baseDiskURL: URL? = nil,
        dataRootURL: URL? = nil,
        capability: String = LishSessionController.makeCapability(),
        vmID: String = UUID().uuidString.replacingOccurrences(of: "-", with: ""),
        fileManager: FileManager = .default
    ) throws {
        self.fileManager = fileManager
        self.capability = capability
        self.vmID = vmID

        let resolvedDataRoot: URL
        if let dataRootURL {
            resolvedDataRoot = dataRootURL.standardizedFileURL
        } else if let support = fileManager.urls(for: .applicationSupportDirectory, in: .userDomainMask).first {
            resolvedDataRoot = support.appendingPathComponent("Lish", isDirectory: true)
        } else {
            throw LishSessionError.noApplicationSupport
        }
        self.dataRootURL = resolvedDataRoot

        let resolvedAssets = assetRootURL ?? Self.defaultAssetRoot(fileManager: fileManager)
        self.assetRootURL = resolvedAssets.standardizedFileURL

        if let baseDiskURL {
            self.baseDiskURL = baseDiskURL.standardizedFileURL
        } else if let environmentPath = ProcessInfo.processInfo.environment["LISH_BASE_DISK"] {
            self.baseDiskURL = URL(fileURLWithPath: environmentPath).standardizedFileURL
        } else {
            self.baseDiskURL = Self.findBaseDisk(in: self.assetRootURL)
        }
        super.init()
    }

    deinit {
        // NSObject deinitialization can happen off the main actor. The service
        // objects are thread-safe and their stop methods are synchronous.
        assetServer?.stop()
        diskServer?.stop()
        networkServer?.stop()
        diskStore?.close()
    }

    public func start() async throws -> LishWebConfiguration {
        guard state == .idle || state == .suspended || state == .failed else {
            throw LishSessionError.invalidState(state, operation: "start")
        }
        stopServices()
        state = .preparing
        do {
            try validateAssets()
            try prepareDisk()
            let assetServer = try LoopbackAssetHTTPServer(
                configuration: AssetHTTPServerConfiguration(
                    rootURL: assetRootURL,
                    capability: capability
                )
            )
            self.assetServer = assetServer
            let assetPort = try await start(assetServer)
            let origin = URL(string: "http://127.0.0.1:\(assetPort)")!

            let store = try DiskStore(url: writableDiskURL)
            let diskConfiguration = try DiskHTTPServerConfiguration(
                capability: capability,
                vmID: vmID,
                allowedOrigin: origin.absoluteString
            )
            let diskServer = DiskHTTPServer(store: store, configuration: diskConfiguration)
            self.diskStore = store
            self.diskServer = diskServer
            let diskPort = try await start(diskServer)

            let networkConfiguration = try RawEthernetServerConfiguration(
                capability: capability,
                allowedOrigin: origin.absoluteString,
                port: 0,
                queueCapacity: 256,
                disableHostLoopback: true,
                allowRemoteConnections: false
            )
            let networkServer = RawEthernetWebSocketServer(configuration: networkConfiguration)
            self.networkServer = networkServer
            let networkPort = try await start(networkServer)

            let page = assetServer.pageURL(port: assetPort, path: "app/index.html")
            let diskURL = URL(string: "http://127.0.0.1:\(diskPort)\(diskConfiguration.diskPath)")!
            let networkURL = URL(string: "ws://127.0.0.1:\(networkPort)/")!
            let configuration = LishWebConfiguration(
                pageURL: page,
                origin: origin,
                capability: capability,
                vmID: vmID,
                diskURL: diskURL,
                networkURL: networkURL,
                networkProtocols: [RawEthernetServerConfiguration.protocolName, capability]
            )
            webConfiguration = configuration
            state = .loading
            return configuration
        } catch {
            state = .failed
            stopServices()
            throw error
        }
    }

    public func markRunning() {
        guard state == .loading || state == .suspended else { return }
        state = .running
    }

    public func quiesce() async throws {
        guard state == .running || state == .loading else {
            if state == .suspended || state == .idle || state == .destroyed { return }
            throw LishSessionError.invalidState(state, operation: "quiesce")
        }
        state = .quiescing
        do {
            try await flushDisk()
            stopServices()
            state = .suspended
        } catch {
            state = .failed
            throw error
        }
    }

    public func stop() async {
        guard state != .destroyed else { return }
        stopServices()
        state = .idle
        webConfiguration = nil
    }

    public func destroy() async {
        guard state != .destroyed else { return }
        stopServices()
        state = .destroyed
        webConfiguration = nil
    }

    public var writableDiskURL: URL {
        dataRootURL
            .appendingPathComponent("vms", isDirectory: true)
            .appendingPathComponent("default", isDirectory: true)
            .appendingPathComponent("disk.img")
    }

    private func prepareDisk() throws {
        try fileManager.createDirectory(
            at: writableDiskURL.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        if !fileManager.fileExists(atPath: writableDiskURL.path) {
            guard fileManager.fileExists(atPath: baseDiskURL.path) else {
                throw LishSessionError.missingAsset(baseDiskURL.path)
            }
            try DiskImage.cloneOrCopy(from: baseDiskURL, to: writableDiskURL)
        }
        _ = try DiskImage.validate(writableDiskURL)
    }

    private func validateAssets() throws {
        let required = [
            "app/index.html",
            "app/app.mjs",
            "rv64.js",
            "rv64.worker.js",
            "native-disk.js",
            "vendor/xterm/xterm.js",
            "vendor/xterm/xterm.css",
            "vendor/xterm/addon-fit.js",
            "images/alpine/Image",
        ]
        for path in required {
            let url = assetRootURL.appendingPathComponent(path)
            guard fileManager.fileExists(atPath: url.path) else {
                throw LishSessionError.missingAsset(url.path)
            }
        }
        let wasm = assetRootURL.appendingPathComponent("rv64_wasm.wasm")
        if !fileManager.fileExists(atPath: wasm.path) {
            let checkoutWasm = assetRootURL
                .deletingLastPathComponent()
                .appendingPathComponent("target/wasm32-unknown-unknown/release/rv64_wasm.wasm")
            guard fileManager.fileExists(atPath: checkoutWasm.path) else {
                throw LishSessionError.missingAsset(
                    "\(wasm.path) (stage target/wasm32-unknown-unknown/release/rv64_wasm.wasm)"
                )
            }
            do {
                try fileManager.copyItem(at: checkoutWasm, to: wasm)
            } catch {
                throw LishSessionError.missingAsset(
                    "\(wasm.path) (copy \(checkoutWasm.path) into the asset root)"
                )
            }
        }
    }

    private func flushDisk() async throws {
        guard let diskStore else { return }
        try await diskStore.flushAsync()
    }

    private func stopNetwork() {
        networkServer?.stop()
        networkServer = nil
    }

    private func stopServices() {
        stopNetwork()
        diskServer?.stop()
        diskServer = nil
        diskStore?.close()
        diskStore = nil
        assetServer?.stop()
        assetServer = nil
    }

    private func start(_ server: LoopbackAssetHTTPServer) async throws -> UInt16 {
        try await withCheckedThrowingContinuation { continuation in
            server.start { result in continuation.resume(with: result) }
        }
    }

    private func start(_ server: DiskHTTPServer) async throws -> UInt16 {
        try await withCheckedThrowingContinuation { continuation in
            server.start { result in continuation.resume(with: result) }
        }
    }

    private func start(_ server: RawEthernetWebSocketServer) async throws -> UInt16 {
        try await withCheckedThrowingContinuation { continuation in
            server.start { result in continuation.resume(with: result) }
        }
    }

    public static func makeCapability() -> String {
        UUID().uuidString.replacingOccurrences(of: "-", with: "") +
            UUID().uuidString.replacingOccurrences(of: "-", with: "")
    }

    private static func defaultAssetRoot(fileManager: FileManager) -> URL {
        if let environmentPath = ProcessInfo.processInfo.environment["LISH_ASSET_ROOT"] {
            return URL(fileURLWithPath: environmentPath)
        }
        let current = URL(fileURLWithPath: fileManager.currentDirectoryPath, isDirectory: true)
        let candidates = [
            Bundle.main.resourceURL?.appendingPathComponent("web", isDirectory: true),
            current.appendingPathComponent("web", isDirectory: true),
            current.deletingLastPathComponent().appendingPathComponent("web", isDirectory: true),
        ].compactMap { $0 }
        return candidates.first(where: {
            fileManager.fileExists(atPath: $0.appendingPathComponent("app/index.html").path)
        }) ?? current.appendingPathComponent("web")
    }

    private static func findBaseDisk(in root: URL) -> URL {
        let candidates = [
            root.appendingPathComponent("images/alpine/alpine.ext4"),
            root.appendingPathComponent("alpine.ext4"),
            root.appendingPathComponent("disk.img"),
        ]
        return candidates.first(where: { fileManagerExists($0) }) ?? candidates[0]
    }
}

private func fileManagerExists(_ url: URL) -> Bool {
    FileManager.default.fileExists(atPath: url.path)
}
