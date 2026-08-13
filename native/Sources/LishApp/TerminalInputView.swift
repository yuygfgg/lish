import AppKit
import Foundation

/// Native first responder used for committed text and IME composition.
///
/// Rendering remains in WKWebView. This view only owns the platform text
/// input contract, which prevents AppKit's marked text from being sent to the
/// guest before the input method commits it.
public final class TerminalInputView: NSView, @preconcurrency NSTextInputClient, NSUserInterfaceValidations {
    public var sendBytes: (@MainActor (Data) -> Void)?
    public var sendKey: (@MainActor (TerminalKey) -> Void)?
    public var copySelection: (@MainActor () -> Void)?
    public var pasteText: (@MainActor (String) -> Void)?
    public var selectAllTerminal: (@MainActor () -> Void)?
    public var terminalAvailable = false
    public var terminalHasSelection = false

    private var marked = NSMutableAttributedString()
    private var inputSelectedRange = NSRange(location: 0, length: 0)

    public override var acceptsFirstResponder: Bool { true }

    public override func becomeFirstResponder() -> Bool {
        true
    }

    public override func keyDown(with event: NSEvent) {
        if hasMarkedText() {
            interpretKeyEvents([event])
            return
        }
        if performEditKeyEquivalent(event) {
            return
        }
        if event.modifierFlags.contains(.control),
           let characters = event.charactersIgnoringModifiers,
           characters.utf8.count == 1 {
            sendControl(characters)
            return
        }
        switch event.keyCode {
        case 36, 76: sendKey?(.return)
        case 51: sendKey?(.delete)
        case 48: sendKey?(.tab)
        case 53: sendKey?(.escape)
        case 123: sendKey?(.left)
        case 124: sendKey?(.right)
        case 125: sendKey?(.down)
        case 126: sendKey?(.up)
        default:
            interpretKeyEvents([event])
        }
    }

    public func insertText(_ string: Any, replacementRange: NSRange) {
        let text: String?
        if let attributed = string as? NSAttributedString {
            text = attributed.string
        } else if let value = string as? String {
            text = value
        } else {
            text = nil
        }
        guard let text, !text.isEmpty else { return }
        marked.mutableString.setString("")
        inputSelectedRange = NSRange(location: 0, length: 0)
        let terminalText = text
            .replacingOccurrences(of: "\r\n", with: "\n")
            .replacingOccurrences(of: "\n", with: "\r")
        sendBytes?(Data(terminalText.utf8))
    }

    public func setMarkedText(_ markedText: Any, selectedRange: NSRange, replacementRange: NSRange) {
        if let attributed = markedText as? NSAttributedString {
            marked = NSMutableAttributedString(attributedString: attributed)
        } else if let value = markedText as? String {
            marked = NSMutableAttributedString(string: value)
        } else {
            marked = NSMutableAttributedString()
        }
        self.inputSelectedRange = selectedRange
    }

    public func unmarkText() {
        marked.mutableString.setString("")
        inputSelectedRange = NSRange(location: 0, length: 0)
    }

    public func selectedRange() -> NSRange { inputSelectedRange }

    public func markedRange() -> NSRange {
        marked.length == 0 ? NSRange(location: NSNotFound, length: 0) : NSRange(location: 0, length: marked.length)
    }

    public func hasMarkedText() -> Bool { marked.length > 0 }

    public func attributedSubstring(forProposedRange range: NSRange, actualRange: NSRangePointer?) -> NSAttributedString? {
        let valid = NSIntersectionRange(range, NSRange(location: 0, length: marked.length))
        actualRange?.pointee = valid
        guard valid.length > 0 else { return NSAttributedString(string: "") }
        return marked.attributedSubstring(from: valid)
    }

    public func characterIndex(for point: NSPoint) -> Int { 0 }

    public func firstRect(forCharacterRange range: NSRange, actualRange: NSRangePointer?) -> NSRect {
        actualRange?.pointee = range
        guard let window else { return .zero }
        return window.convertToScreen(convert(bounds, to: nil))
    }

    public func validAttributesForMarkedText() -> [NSAttributedString.Key] { [] }

    public override func doCommand(by selector: Selector) {
        switch selector {
        case #selector(NSResponder.insertNewline(_:)): sendKey?(.return)
        case #selector(NSResponder.deleteBackward(_:)): sendKey?(.delete)
        default: break
        }
    }

    @objc public func copy(_ sender: Any?) {
        guard terminalAvailable else { return }
        copySelection?()
    }

    @objc public func paste(_ sender: Any?) {
        guard terminalAvailable else { return }
        guard let text = NSPasteboard.general.string(forType: .string) else { return }
        pasteText?(text)
    }

    public override func selectAll(_ sender: Any?) {
        guard terminalAvailable else { return }
        selectAllTerminal?()
    }

    public func validateUserInterfaceItem(_ item: any NSValidatedUserInterfaceItem) -> Bool {
        switch item.action {
        case #selector(copy(_:)):
            terminalHasSelection
        case #selector(paste(_:)):
            terminalAvailable && NSPasteboard.general.availableType(from: [.string]) != nil
        case #selector(selectAll(_:)):
            terminalAvailable
        default:
            false
        }
    }

    private func performEditKeyEquivalent(_ event: NSEvent) -> Bool {
        let modifiers = event.modifierFlags.intersection([.command, .control, .option, .shift])
        guard modifiers == .command,
              let characters = event.charactersIgnoringModifiers?.lowercased() else { return false }
        let action: Selector
        switch characters {
        case "a": action = #selector(selectAll(_:))
        case "c": action = #selector(copy(_:))
        case "v": action = #selector(paste(_:))
        default: return false
        }
        NSApp.sendAction(action, to: nil, from: self)
        return true
    }

    private func sendControl(_ characters: String) {
        guard let scalar = characters.uppercased().unicodeScalars.first,
              scalar.value < 0x80 else { return }
        let value = scalar.value
        let control = value == 0x3f ? 0x7f : value == 0x20 ? 0 : value & 0x1f
        sendBytes?(Data([UInt8(control)]))
    }
}

public enum TerminalKey: Sendable {
    case `return`
    case delete
    case tab
    case escape
    case left
    case right
    case up
    case down
}
