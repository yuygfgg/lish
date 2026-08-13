const NATIVE_CURSOR_STYLES = Object.freeze({
  focused: "block",
  unfocused: "outline",
});

// AppKit owns text input in the native app, so xterm's textarea is not the
// source of truth for focus. Select the inactive cursor style from AppKit's
// window focus instead.
export function applyNativeCursorFocus(terminal, focused) {
  const style = focused
    ? NATIVE_CURSOR_STYLES.focused
    : NATIVE_CURSOR_STYLES.unfocused;
  if (terminal.options.cursorInactiveStyle !== style) {
    terminal.options.cursorInactiveStyle = style;
  }
}
