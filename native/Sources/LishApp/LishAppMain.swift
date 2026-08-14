import AppKit
import WebKit

@main
@MainActor
final class LishApplicationDelegate: NSObject, NSApplicationDelegate, WKNavigationDelegate, WKScriptMessageHandler,
    NSMenuItemValidation {
    private static let bridgeName = "lish"

    private var window: NSWindow?
    private var webView: WKWebView?
    private var inputView: TerminalInputView?
    private var session: LishSessionController?
    private var statusField: NSTextField?
    private var terminalInputFocused = false
    private var pageState = "cold"
    private var instructionsPerSecond: Double?
    private var jitPending: Int?
    private var terminationPending = false
    private var nextControlRequest = 1

    static func main() {
        let application = NSApplication.shared
        let delegate = LishApplicationDelegate()
        application.delegate = delegate
        application.setActivationPolicy(.regular)
        application.mainMenu = delegate.makeMainMenu()
        withExtendedLifetime(delegate) {
            application.run()
        }
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        diagnostic("application launched")
        do {
            let session = try LishSessionController()
            self.session = session
            let contentController = WKUserContentController()
            contentController.add(self, name: Self.bridgeName)
            let webConfiguration = WKWebViewConfiguration()
            webConfiguration.userContentController = contentController
            webConfiguration.preferences.javaScriptCanOpenWindowsAutomatically = false

            let webView = WKWebView(frame: .zero, configuration: webConfiguration)
            webView.translatesAutoresizingMaskIntoConstraints = false
            webView.navigationDelegate = self
            self.webView = webView

            let input = TerminalInputView(frame: .zero)
            input.translatesAutoresizingMaskIntoConstraints = false
            input.sendBytes = { [weak self] bytes in
                self?.sendInput(bytes)
            }
            input.sendKey = { [weak self] key in
                self?.sendKey(key)
            }
            input.copySelection = { [weak self] in
                self?.copyTerminalSelection()
            }
            input.pasteText = { [weak self] text in
                self?.pasteTerminalText(text)
            }
            input.selectAllTerminal = { [weak self] in
                self?.selectAllTerminal()
            }
            input.focusChanged = { [weak self] focused in
                self?.terminalInputFocused = focused
                self?.updateTerminalFocus()
            }
            self.inputView = input

            let root = NSView()
            root.addSubview(webView)
            root.addSubview(input)
            NSLayoutConstraint.activate([
                webView.leadingAnchor.constraint(equalTo: root.leadingAnchor),
                webView.trailingAnchor.constraint(equalTo: root.trailingAnchor),
                webView.topAnchor.constraint(equalTo: root.topAnchor),
                webView.bottomAnchor.constraint(equalTo: root.bottomAnchor),
                input.leadingAnchor.constraint(equalTo: root.leadingAnchor),
                input.bottomAnchor.constraint(equalTo: root.bottomAnchor),
                input.widthAnchor.constraint(equalToConstant: 1),
                input.heightAnchor.constraint(equalToConstant: 1),
            ])

            let window = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 960, height: 640),
                styleMask: [.titled, .closable, .miniaturizable, .resizable],
                backing: .buffered,
                defer: false
            )
            window.title = "Lish"
            window.contentView = root
            window.center()
            window.minSize = NSSize(width: 520, height: 360)
            window.isReleasedWhenClosed = false
            window.delegate = self
            self.window = window
            installStatusAccessory(in: window)
            window.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)

            Task { [weak self] in
                await self?.loadSession()
            }
        } catch {
            showStartupError(error)
        }
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard let session, session.state != .idle, session.state != .destroyed else {
            return .terminateNow
        }
        guard !terminationPending else { return .terminateLater }
        terminationPending = true
        diagnostic("termination quiesce started")
        Task { [weak self] in
            await self?.quiesceWebPage()
            try? await self?.session?.quiesce()
            self?.diagnostic("termination quiesce finished")
            NSApp.reply(toApplicationShouldTerminate: true)
        }
        return .terminateLater
    }

    func applicationWillTerminate(_ notification: Notification) {
        webView?.stopLoading()
        webView?.configuration.userContentController.removeScriptMessageHandler(forName: Self.bridgeName)
        Task { [session] in await session?.destroy() }
    }

    func userContentController(
        _ userContentController: WKUserContentController,
        didReceive message: WKScriptMessage
    ) {
        guard message.name == Self.bridgeName,
              let body = message.body as? [String: Any],
              (body["version"] as? Int) == 1 else { return }
        if body["type"] as? String == "event" {
            handlePageEvent(body)
            return
        }
        guard body["type"] as? String == "request",
              let operation = body["op"] as? String else { return }
        Task { [weak self] in
            await self?.handleControl(operation: operation, body: body)
        }
    }

    func webView(
        _ webView: WKWebView,
        decidePolicyFor navigationAction: WKNavigationAction,
        decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy) -> Void
    ) {
        guard let url = navigationAction.request.url,
              let origin = session?.webConfiguration?.origin,
              url.scheme == origin.scheme,
              url.host == origin.host,
              url.port == origin.port,
              url.path.hasPrefix("/s/\(session?.capability ?? "")/assets/") else {
            decisionHandler(.cancel)
            return
        }
        decisionHandler(.allow)
    }

    private func loadSession() async {
        pageState = "loading"
        updateStatusField()
        do {
            guard let session else { return }
            let configuration = try await session.start()
            diagnostic("loopback services ready")
            webView?.load(URLRequest(url: configuration.pageURL))
        } catch {
            showStartupError(error)
        }
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        guard let configuration = session?.webConfiguration,
              webView.url == configuration.pageURL else { return }
        diagnostic("guest page loaded")
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        guard !terminationPending,
              let configuration = session?.webConfiguration else { return }
        inputView?.terminalAvailable = false
        inputView?.terminalHasSelection = false
        pageState = "loading"
        instructionsPerSecond = nil
        jitPending = nil
        updateStatusField()
        webView.load(URLRequest(url: configuration.pageURL))
    }

    private func handleControl(operation: String, body: [String: Any]) async {
        guard let session else { return }
        do {
            switch operation {
            case "focus-input":
                window?.makeFirstResponder(inputView)
            case "start":
                session.markRunning()
            case "quiesce":
                try await session.quiesce()
            case "destroy":
                await session.destroy()
            default:
                throw LishApplicationError.unknownNativeOperation(operation)
            }
            sendControlResult(body: body, result: .success([:]))
        } catch {
            sendControlResult(body: body, result: .failure(error))
        }
    }

    private func sendInput(_ bytes: Data) {
        guard let webView else { return }
        webView.callAsyncJavaScript(
            "window.lishNativeInput(Uint8Array.from(bytes))",
            arguments: ["bytes": Array(bytes)],
            in: nil,
            in: .page,
            completionHandler: nil
        )
    }

    private func quiesceWebPage() async {
        guard let webView else { return }
        let requestID = "native-\(nextControlRequest)"
        nextControlRequest += 1
        let completed = await withCheckedContinuation { (continuation: CheckedContinuation<Bool, Never>) in
            let result = OneShotResult(continuation)
            webView.callAsyncJavaScript(
                "return await window.lishControl(request)",
                arguments: [
                    "request": [
                        "version": 1,
                        "type": "request",
                        "id": requestID,
                        "op": "quiesce",
                        "payload": [:],
                    ],
                ],
                in: nil,
                in: .page
            ) { _ in
                result.resume(returning: true)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + .seconds(2)) {
                result.resume(returning: false)
            }
        }
        if !completed { webView.stopLoading() }
    }

    private func sendKey(_ key: TerminalKey) {
        let name: String
        switch key {
        case .return: name = "return"
        case .delete: name = "backspace"
        case .tab: name = "tab"
        case .escape: name = "escape"
        case .left: name = "left"
        case .right: name = "right"
        case .up: name = "up"
        case .down: name = "down"
        }
        webView?.callAsyncJavaScript(
            "window.lishInput(key)",
            arguments: ["key": ["kind": "key", "key": name]],
            in: nil,
            in: .page,
            completionHandler: nil
        )
    }

    private func copyTerminalSelection() {
        guard let webView else { return }
        webView.callAsyncJavaScript(
            "return window.lishCopySelection()",
            arguments: [:],
            in: nil,
            in: .page
        ) { [weak self] result in
            guard case .success(let value) = result,
                  let text = value as? String,
                  !text.isEmpty else {
                self?.inputView?.terminalHasSelection = false
                return
            }
            let pasteboard = NSPasteboard.general
            pasteboard.clearContents()
            pasteboard.setString(text, forType: .string)
        }
    }

    private func pasteTerminalText(_ text: String) {
        webView?.callAsyncJavaScript(
            "window.lishNativePaste(text)",
            arguments: ["text": text],
            in: nil,
            in: .page,
            completionHandler: nil
        )
    }

    private func selectAllTerminal() {
        webView?.callAsyncJavaScript(
            "window.lishSelectAll()",
            arguments: [:],
            in: nil,
            in: .page,
            completionHandler: nil
        )
    }

    private func updateTerminalFocus() {
        guard let webView else { return }
        webView.callAsyncJavaScript(
            "window.lishNativeWindowFocus(focused)",
            arguments: ["focused": terminalInputFocused && window?.isKeyWindow == true],
            in: nil,
            in: .page,
            completionHandler: nil
        )
    }

    private func installStatusAccessory(in window: NSWindow) {
        let field = NSTextField(labelWithString: "Waiting")
        field.alignment = .right
        field.font = .monospacedDigitSystemFont(ofSize: 11, weight: .regular)
        field.textColor = .secondaryLabelColor
        field.translatesAutoresizingMaskIntoConstraints = false

        let container = NSView(frame: NSRect(x: 0, y: 0, width: 300, height: 24))
        container.addSubview(field)
        NSLayoutConstraint.activate([
            container.widthAnchor.constraint(equalToConstant: 300),
            field.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 8),
            field.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -8),
            field.centerYAnchor.constraint(equalTo: container.centerYAnchor),
        ])

        let accessory = NSTitlebarAccessoryViewController()
        accessory.layoutAttribute = .right
        accessory.view = container
        window.addTitlebarAccessoryViewController(accessory)
        statusField = field
        updateStatusField()
    }

    private func updateStatusField() {
        let state = Self.pageStateLabel(pageState)
        guard pageState == "running" else {
            statusField?.stringValue = state
            return
        }
        let pending = jitPending.map(String.init) ?? "--"
        let rate = instructionsPerSecond.map(Self.formatInstructionRate) ?? "-- MIPS"
        statusField?.stringValue = "\(state) · JIT \(pending) pending · \(rate)"
    }

    private static func pageStateLabel(_ state: String) -> String {
        switch state {
        case "cold": return "Waiting"
        case "loading": return "Loading"
        case "starting": return "Starting"
        case "running": return "Running"
        case "stopping": return "Stopping"
        case "stopped": return "Stopped"
        case "quiescing": return "Pausing"
        case "suspended": return "Paused"
        case "failed": return "Failed"
        case "destroyed": return "Ended"
        default: return state.capitalized
        }
    }

    private static func formatInstructionRate(_ value: Double) -> String {
        let rate = max(0, value)
        if rate >= 1_000_000_000 { return String(format: "%.2f GIPS", rate / 1_000_000_000) }
        if rate >= 1_000_000 { return String(format: "%.1f MIPS", rate / 1_000_000) }
        if rate >= 1_000 { return String(format: "%.1f KIPS", rate / 1_000) }
        return String(format: "%.0f IPS", rate)
    }

    private func performPageOperation(_ operation: String) {
        guard let webView else { return }
        let requestID = "native-\(nextControlRequest)"
        nextControlRequest += 1
        webView.callAsyncJavaScript(
            "return await window.lishControl(request)",
            arguments: [
                "request": [
                    "version": 1,
                    "type": "request",
                    "id": requestID,
                    "op": operation,
                    "payload": [:],
                ],
            ],
            in: nil,
            in: .page
        ) { [weak self] result in
            if case .failure(let error) = result {
                self?.diagnostic("machine operation failed: \(operation): \(error.localizedDescription)")
            }
        }
    }

    @objc private func startMachine(_ sender: Any?) {
        performPageOperation("start")
    }

    @objc private func stopMachine(_ sender: Any?) {
        performPageOperation("stop")
    }

    @objc private func resetMachine(_ sender: Any?) {
        performPageOperation("reset")
    }

    @objc private func clearTerminal(_ sender: Any?) {
        performPageOperation("clear-terminal")
    }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        switch menuItem.action {
        case #selector(startMachine(_:)):
            return pageState == "stopped" || pageState == "suspended"
        case #selector(stopMachine(_:)):
            return pageState == "running" || pageState == "starting"
        case #selector(resetMachine(_:)):
            return ["running", "stopped", "suspended"].contains(pageState)
        case #selector(clearTerminal(_:)):
            return inputView?.terminalAvailable == true
        default:
            return true
        }
    }

    private func handlePageEvent(_ body: [String: Any]) {
        guard let event = body["event"] as? String else { return }
        switch event {
        case "page-ready":
            guard let configuration = session?.webConfiguration,
                  let webView,
                  webView.url == configuration.pageURL else { return }
            inputView?.terminalAvailable = true
            inputView?.terminalHasSelection = false
            updateTerminalFocus()
            diagnostic("guest page runtime ready")
            bootstrapPage(configuration, in: webView)
        case "focus-input":
            window?.makeFirstResponder(inputView)
            updateTerminalFocus()
        case "ready":
            diagnostic("guest machine ready")
            window?.makeFirstResponder(inputView)
            updateTerminalFocus()
        case "state":
            if let payload = body["payload"] as? [String: Any],
               let state = payload["state"] as? String {
                pageState = state
                if state != "running" {
                    instructionsPerSecond = nil
                    jitPending = nil
                }
                updateStatusField()
                diagnostic("guest state: \(state)")
            }
        case "telemetry":
            guard let payload = body["payload"] as? [String: Any] else { return }
            instructionsPerSecond = (payload["instructionsPerSecond"] as? NSNumber)?.doubleValue
            jitPending = (payload["jitPending"] as? NSNumber)?.intValue
            updateStatusField()
            diagnosticTelemetry(payload)
        case "selection-change":
            guard let payload = body["payload"] as? [String: Any],
                  let hasSelection = payload["hasSelection"] as? Bool else { return }
            inputView?.terminalHasSelection = hasSelection
        case "disk-error":
            guard ProcessInfo.processInfo.environment["LISH_DIAGNOSTICS"] != nil,
                  let payload = body["payload"] as? [String: Any] else { return }
            let kind = payload["kind"] as? String ?? "unknown"
            let offset = payload["offset"] as? String ?? "unknown"
            let length = payload["length"] as? String ?? "unknown"
            let error = payload["error"] as? String ?? "unknown"
            diagnostic(
                "disk request failed: kind=\(kind) offset=\(offset) " +
                    "length=\(length) error=\(redacted(error))"
            )
        default:
            break
        }
    }

    private func sendControlResult(body: [String: Any], result: Result<[String: Any], Error>) {
        guard let requestID = body["id"] as? String,
              let operation = body["op"] as? String,
              let webView else { return }
        var response: [String: Any] = [
            "version": 1,
            "type": "response",
            "id": requestID,
            "op": operation,
        ]
        switch result {
        case .success(let payload):
            response["ok"] = true
            response["payload"] = payload
        case .failure(let error):
            response["ok"] = false
            response["error"] = [
                "name": "NativeError",
                "message": String(describing: error),
            ]
        }
        webView.callAsyncJavaScript(
            "window.lishNativeControlResult(response)",
            arguments: ["response": response],
            in: nil,
            in: .page,
            completionHandler: nil
        )
    }

    private func bootstrapPage(_ configuration: LishWebConfiguration, in webView: WKWebView) {
        let pageConfiguration: [String: Any] = [
            "vmID": configuration.vmID,
            "diskURL": configuration.diskURL.absoluteString,
            "networkURL": configuration.networkURL.absoluteString,
            "capability": configuration.capability,
            "networkProtocols": configuration.networkProtocols,
            "inputMode": "native",
            "diagnostics": ProcessInfo.processInfo.environment["LISH_DIAGNOSTICS"] != nil,
            "jit": bootstrapJITConfiguration(),
        ]
        webView.callAsyncJavaScript(
            """
            try {
                await window.lishBootstrap(configuration);
                return { ok: true };
            } catch (error) {
                return {
                    ok: false,
                    name: String(error?.name ?? "Error"),
                    message: String(error?.message ?? error),
                };
            }
            """,
            arguments: ["configuration": pageConfiguration],
            in: nil,
            in: .page
        ) { [weak self, weak webView] result in
            guard let self,
                  webView?.url == configuration.pageURL,
                  !self.terminationPending else { return }
            switch result {
            case .success(let value):
                guard let response = value as? [String: Any],
                      response["ok"] as? Bool == false else { return }
                let name = response["name"] as? String ?? "Error"
                let message = response["message"] as? String ?? "Unknown page error"
                self.diagnostic("page bootstrap error: \(self.redacted("\(name): \(message)"))")
            case .failure(let error):
                let nsError = error as NSError
                if nsError.domain == WKError.errorDomain,
                   nsError.code == WKError.webContentProcessTerminated.rawValue {
                    return
                }
                self.diagnostic("page bootstrap bridge error: \(self.redacted(error.localizedDescription))")
            }
            self.showStartupError(LishApplicationError.pageBootstrapFailed)
        }
    }

    private func bootstrapJITConfiguration() -> [String: Any] {
        let environment = ProcessInfo.processInfo.environment
        var configuration: [String: Any] = [:]

        if let value = environmentBoolean(named: "LISH_JIT_CONFIRMED_BATCH", environment: environment) {
            configuration["confirmedBatch"] = value
        }
        if let value = environmentBoolean(named: "LISH_JIT_PAGE_MODULES", environment: environment) {
            configuration["pageModules"] = value
        }
        if let value = environmentInteger(
            named: "LISH_JIT_CONFIRMED_BATCH_TARGET",
            minimum: 2,
            environment: environment
        ) {
            configuration["confirmedBatchTarget"] = value
        }
        if let value = environmentInteger(
            named: "LISH_JIT_THRESHOLD",
            minimum: 1,
            environment: environment
        ) {
            configuration["threshold"] = value
        }
        if let value = environmentInteger(
            named: "LISH_JIT_ASYNC_COMPILERS",
            minimum: 1,
            maximum: 4,
            environment: environment
        ) {
            configuration["asyncCompilers"] = value
        }
        return configuration
    }

    private func environmentBoolean(named name: String, environment: [String: String]) -> Bool? {
        guard let value = environment[name] else { return nil }
        switch value {
        case "0": return false
        case "1": return true
        default:
            diagnostic("ignored invalid \(name)=\(String(reflecting: value)); expected 0 or 1")
            return nil
        }
    }

    private func environmentInteger(
        named name: String,
        minimum: Int,
        maximum: Int = Int(UInt32.max),
        environment: [String: String]
    ) -> Int? {
        guard let rawValue = environment[name] else { return nil }
        guard let value = Int(rawValue), value >= minimum, value <= maximum else {
            diagnostic(
                "ignored invalid \(name)=\(String(reflecting: rawValue)); " +
                    "expected an integer from \(minimum) through \(maximum)"
            )
            return nil
        }
        return value
    }

    private func diagnosticTelemetry(_ payload: [String: Any]) {
        guard ProcessInfo.processInfo.environment["LISH_DIAGNOSTICS"] != nil else { return }
        let metrics = payload["jitMetrics"] as? [String: Any]
        let instructionsPerSecond = (payload["instructionsPerSecond"] as? NSNumber)?.doubleValue
        diagnostic(
            "telemetry mips=\(Self.formatTelemetryRate(instructionsPerSecond)) " +
                "cacheEntries=\(Self.formatTelemetryInteger(metrics?["rustCacheEntries"])) " +
                "liveModules=\(Self.formatTelemetryInteger(metrics?["liveModules"])) " +
                "registeredModules=\(Self.formatTelemetryInteger(metrics?["registeredModules"])) " +
                "retiredModules=\(Self.formatTelemetryInteger(metrics?["retiredModules"])) " +
                "evictedModules=\(Self.formatTelemetryInteger(metrics?["evictedModules"])) " +
                "liveBytesMiB=\(Self.formatTelemetryMiB(metrics?["liveBytes"])) " +
                "emittedBytesMiB=\(Self.formatTelemetryMiB(metrics?["emittedBytes"])) " +
                "dispatches=\(Self.formatTelemetryInteger(metrics?["rustDispatches"])) " +
                "interpInsns=\(Self.formatTelemetryInteger(metrics?["rustInterpreterInstructions"])) " +
                "asyncCompileMs=\(Self.formatTelemetryDecimal(metrics?["asyncCompileMs"])) " +
                "asyncCompileCount=\(Self.formatTelemetryInteger(metrics?["asyncCompileCount"])) " +
                "maxAsyncCompileMs=\(Self.formatTelemetryDecimal(metrics?["maxAsyncCompileMs"])) " +
                "asyncActive=\(Self.formatTelemetryInteger(metrics?["asyncCompileActive"])) " +
                "asyncQueued=\(Self.formatTelemetryInteger(metrics?["asyncCompileQueued"])) " +
                "pageModules=\(Self.formatTelemetryInteger(metrics?["pageModulesLanded"])) " +
                "pageMembers=\(Self.formatTelemetryInteger(metrics?["pageModuleMembers"])) " +
                "confirmedStaged=\(Self.formatTelemetryInteger(metrics?["confirmedStaged"])) " +
                "capacityRejects=\(Self.formatTelemetryInteger(metrics?["capacityRejects"])) " +
                "rustCapacityRejects=\(Self.formatTelemetryInteger(metrics?["rustCapacityRejects"])) " +
                "rustEvictedOwners=\(Self.formatTelemetryInteger(metrics?["rustEvictedOwners"])) " +
                "cooledEntries=\(Self.formatTelemetryInteger(metrics?["evictionCooledEntries"])) " +
                "pending=\(Self.formatTelemetryInteger(payload["jitPending"]))"
        )
    }

    private static func formatTelemetryRate(_ instructionsPerSecond: Double?) -> String {
        guard let instructionsPerSecond, instructionsPerSecond.isFinite else { return "--" }
        return String(format: "%.2f", max(0, instructionsPerSecond) / 1_000_000)
    }

    private static func formatTelemetryInteger(_ value: Any?) -> String {
        guard let number = value as? NSNumber else { return "--" }
        return String(number.int64Value)
    }

    private static func formatTelemetryDecimal(_ value: Any?) -> String {
        guard let number = value as? NSNumber, number.doubleValue.isFinite else { return "--" }
        return String(format: "%.2f", number.doubleValue)
    }

    private static func formatTelemetryMiB(_ value: Any?) -> String {
        guard let number = value as? NSNumber, number.doubleValue.isFinite else { return "--" }
        return String(format: "%.2f", number.doubleValue / (1024 * 1024))
    }

    private func showStartupError(_ error: Error) {
        pageState = "failed"
        instructionsPerSecond = nil
        jitPending = nil
        updateStatusField()
        diagnostic("startup failed")
        let alert = NSAlert()
        alert.messageText = "Lish could not start"
        alert.informativeText = String(describing: error)
        alert.alertStyle = .critical
        alert.addButton(withTitle: "Quit")
        alert.runModal()
        NSApp.terminate(nil)
    }

    private func diagnostic(_ message: String) {
        guard ProcessInfo.processInfo.environment["LISH_DIAGNOSTICS"] != nil else { return }
        FileHandle.standardError.write(Data("lish-app: \(message)\n".utf8))
    }

    private func redacted(_ message: String) -> String {
        guard let capability = session?.capability else { return message }
        return message.replacingOccurrences(of: capability, with: "<capability>")
    }

    private func makeMainMenu() -> NSMenu {
        let mainMenu = NSMenu()

        let applicationItem = NSMenuItem(title: "Lish", action: nil, keyEquivalent: "")
        let applicationMenu = NSMenu(title: "Lish")
        applicationMenu.addItem(
            withTitle: "About Lish",
            action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)),
            keyEquivalent: ""
        )
        applicationMenu.addItem(.separator())

        let machineItem = NSMenuItem(title: "Machine", action: nil, keyEquivalent: "")
        let machineMenu = NSMenu(title: "Machine")
        let start = machineMenu.addItem(
            withTitle: "Start",
            action: #selector(startMachine(_:)),
            keyEquivalent: ""
        )
        start.target = self
        let stop = machineMenu.addItem(
            withTitle: "Stop",
            action: #selector(stopMachine(_:)),
            keyEquivalent: ""
        )
        stop.target = self
        let reset = machineMenu.addItem(
            withTitle: "Reset",
            action: #selector(resetMachine(_:)),
            keyEquivalent: "r"
        )
        reset.keyEquivalentModifierMask = [.command, .option]
        reset.target = self
        machineMenu.addItem(.separator())
        let clear = machineMenu.addItem(
            withTitle: "Clear Terminal",
            action: #selector(clearTerminal(_:)),
            keyEquivalent: "k"
        )
        clear.target = self
        machineItem.submenu = machineMenu
        applicationMenu.addItem(machineItem)
        applicationMenu.addItem(.separator())

        applicationMenu.addItem(
            withTitle: "Hide Lish",
            action: #selector(NSApplication.hide(_:)),
            keyEquivalent: "h"
        )
        let hideOthers = applicationMenu.addItem(
            withTitle: "Hide Others",
            action: #selector(NSApplication.hideOtherApplications(_:)),
            keyEquivalent: "h"
        )
        hideOthers.keyEquivalentModifierMask = [.command, .option]
        applicationMenu.addItem(
            withTitle: "Show All",
            action: #selector(NSApplication.unhideAllApplications(_:)),
            keyEquivalent: ""
        )
        applicationMenu.addItem(.separator())
        applicationMenu.addItem(
            withTitle: "Quit Lish",
            action: #selector(NSApplication.terminate(_:)),
            keyEquivalent: "q"
        )
        applicationItem.submenu = applicationMenu
        mainMenu.addItem(applicationItem)

        let editItem = NSMenuItem(title: "Edit", action: nil, keyEquivalent: "")
        let editMenu = NSMenu(title: "Edit")
        editMenu.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        let redo = editMenu.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "Z")
        redo.keyEquivalentModifierMask = [.command, .shift]
        editMenu.addItem(.separator())
        editMenu.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        editMenu.addItem(withTitle: "Copy", action: #selector(TerminalInputView.copy(_:)), keyEquivalent: "c")
        editMenu.addItem(withTitle: "Paste", action: #selector(TerminalInputView.paste(_:)), keyEquivalent: "v")
        editMenu.addItem(.separator())
        editMenu.addItem(
            withTitle: "Select All",
            action: #selector(TerminalInputView.selectAll(_:)),
            keyEquivalent: "a"
        )
        editItem.submenu = editMenu
        mainMenu.addItem(editItem)

        return mainMenu
    }
}

private enum LishApplicationError: Error, CustomStringConvertible {
    case pageBootstrapFailed
    case unknownNativeOperation(String)

    var description: String {
        switch self {
        case .pageBootstrapFailed:
            return "The guest page could not initialize."
        case .unknownNativeOperation(let operation):
            return "Unknown native operation: \(operation)"
        }
    }
}

private final class OneShotResult<Value: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Value, Never>?

    init(_ continuation: CheckedContinuation<Value, Never>) {
        self.continuation = continuation
    }

    func resume(returning value: Value) {
        lock.lock()
        let current = continuation
        continuation = nil
        lock.unlock()
        current?.resume(returning: value)
    }
}

extension LishApplicationDelegate: NSWindowDelegate {
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    func windowDidBecomeKey(_ notification: Notification) {
        updateTerminalFocus()
    }

    func windowDidResignKey(_ notification: Notification) {
        updateTerminalFocus()
    }
}
