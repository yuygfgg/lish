import AppKit
import WebKit

@main
@MainActor
final class LishApplicationDelegate: NSObject, NSApplicationDelegate, WKNavigationDelegate, WKScriptMessageHandler {
    private static let bridgeName = "lish"

    private var window: NSWindow?
    private var webView: WKWebView?
    private var inputView: TerminalInputView?
    private var session: LishSessionController?
    private var terminationPending = false
    private var nextControlRequest = 1

    static func main() {
        let application = NSApplication.shared
        let delegate = LishApplicationDelegate()
        application.delegate = delegate
        application.setActivationPolicy(.regular)
        application.mainMenu = makeMainMenu()
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

    private func handlePageEvent(_ body: [String: Any]) {
        guard let event = body["event"] as? String else { return }
        switch event {
        case "page-ready":
            guard let configuration = session?.webConfiguration,
                  let webView,
                  webView.url == configuration.pageURL else { return }
            inputView?.terminalAvailable = true
            inputView?.terminalHasSelection = false
            diagnostic("guest page runtime ready")
            bootstrapPage(configuration, in: webView)
        case "focus-input":
            window?.makeFirstResponder(inputView)
        case "ready":
            diagnostic("guest machine ready")
            window?.makeFirstResponder(inputView)
        case "state":
            if let payload = body["payload"] as? [String: Any],
               let state = payload["state"] as? String {
                diagnostic("guest state: \(state)")
            }
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

    private func showStartupError(_ error: Error) {
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

    private static func makeMainMenu() -> NSMenu {
        let mainMenu = NSMenu()

        let applicationItem = NSMenuItem(title: "Lish", action: nil, keyEquivalent: "")
        let applicationMenu = NSMenu(title: "Lish")
        applicationMenu.addItem(
            withTitle: "About Lish",
            action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)),
            keyEquivalent: ""
        )
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
}
