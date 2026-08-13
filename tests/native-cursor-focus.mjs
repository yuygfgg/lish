import assert from "node:assert/strict";
import { applyNativeCursorFocus } from "../web/app/cursor-focus.mjs";

const terminal = { options: { cursorInactiveStyle: "outline" } };

applyNativeCursorFocus(terminal, true);
assert.equal(terminal.options.cursorInactiveStyle, "block");

applyNativeCursorFocus(terminal, false);
assert.equal(terminal.options.cursorInactiveStyle, "outline");

console.log("native cursor focus self-test: PASS");
