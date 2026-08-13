import { RV64 } from "../rv64.js";
import { Terminal } from "../vendor/xterm/xterm.js";
import { FitAddon } from "../vendor/xterm/addon-fit.js";

const PROTOCOL_VERSION = 1;
const DEFAULT_MAX_OUTPUT_BYTES = 2 * 1024 * 1024;
const DEFAULT_WRITE_CREDIT = 256 * 1024;
const DEV_QUERY = "dev";

globalThis.LISH_DIAGNOSTICS = false;

const elements = {
  terminal: document.querySelector("#terminal"),
  loading: document.querySelector("#loading"),
  loadingTitle: document.querySelector("#loading-title"),
  loadingDetail: document.querySelector("#loading-detail"),
  state: document.querySelector("#state"),
  connection: document.querySelector("#connection"),
  queue: document.querySelector("#queue"),
  dimensions: document.querySelector("#dimensions"),
};

const terminal = new Terminal({
  allowProposedApi: false,
  convertEol: false,
  cursorBlink: true,
  fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
  fontSize: 14,
  lineHeight: 1.2,
  scrollback: 5000,
  theme: {
    background: "#070908",
    foreground: "#d7ded9",
    cursor: "#71d59b",
    cursorAccent: "#070908",
    selectionBackground: "#365a46",
  },
});
const fitAddon = new FitAddon();
terminal.loadAddon(fitAddon);
terminal.open(elements.terminal);

function serializedError(error) {
  return {
    message: String(error?.message ?? error),
    name: String(error?.name ?? "Error"),
  };
}

function asBytes(value) {
  if (value instanceof Uint8Array) return value;
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  if (typeof value === "string") return new TextEncoder().encode(value);
  throw new TypeError("input must be text or bytes");
}

function numberOr(value, fallback) {
  return Number.isFinite(Number(value)) ? Number(value) : fallback;
}

function makeMessage(type, fields = {}) {
  return { version: PROTOCOL_VERSION, type, ...fields };
}

function controlResponse(id, op, ok, payloadOrError) {
  return makeMessage("response", {
    id,
    requestId: id,
    op,
    ok,
    ...(ok ? { payload: payloadOrError } : { error: serializedError(payloadOrError) }),
  });
}

class NativeBridge {
  #handler;
  #nextId = 1;
  #pending = new Map();

  constructor() {
    const handlers = globalThis.webkit?.messageHandlers;
    this.#handler = handlers?.lish ?? null;
  }

  get connected() { return this.#handler !== null; }

  post(message) {
    if (!this.#handler) return false;
    try {
      this.#handler.postMessage(message);
      return true;
    } catch (error) {
      console.error("Lish native bridge failed", error);
      return false;
    }
  }

  event(event, payload = {}) {
    return this.post(makeMessage("event", { event, payload }));
  }

  request(op, payload = {}, timeoutMs = 5000) {
    if (!this.#handler) return Promise.resolve(null);
    const id = `web-${this.#nextId++}`;
    const message = makeMessage("request", {
      id,
      op,
      payload,
    });
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        reject(new Error(`native request timed out: ${op}`));
      }, timeoutMs);
      this.#pending.set(id, { resolve, reject, timer });
      if (!this.post(message)) {
        clearTimeout(timer);
        this.#pending.delete(id);
        reject(new Error("native bridge is unavailable"));
      }
    });
  }

  receive(message) {
    if (!message || typeof message !== "object") return false;
    if (Number(message.version) !== PROTOCOL_VERSION) return false;
    const id = message.id ?? message.requestId;
    if (message.type === "response" && id !== undefined) {
      const pending = this.#pending.get(id);
      if (!pending) return true;
      this.#pending.delete(id);
      clearTimeout(pending.timer);
      if (message.ok === false || message.error) pending.reject(new Error(message.error?.message ?? String(message.error)));
      else pending.resolve(message.payload);
      return true;
    }
    return false;
  }
}

class OutputQueue {
  #terminal;
  #maxBytes;
  #credit;
  #queued = 0;
  #chunks = [];
  #writing = false;
  #onChange;
  #onOverflow;

  constructor(outputTerminal, { maxBytes, credit, onChange, onOverflow }) {
    this.#terminal = outputTerminal;
    this.#maxBytes = maxBytes;
    this.#credit = credit;
    this.#onChange = onChange;
    this.#onOverflow = onOverflow;
  }

  get queuedBytes() { return this.#queued; }
  get credit() { return this.#credit; }

  clear() {
    this.#chunks = [];
    this.#queued = 0;
    this.#notify();
  }

  push(value) {
    const bytes = asBytes(value).slice();
    if (bytes.length === 0) return;
    if (bytes.length > this.#maxBytes || this.#queued + bytes.length > this.#maxBytes) {
      this.#onOverflow?.(bytes.length);
      return;
    }
    this.#chunks.push(bytes);
    this.#queued += bytes.length;
    this.#notify();
    this.#pump();
  }

  #pump() {
    if (this.#writing || this.#chunks.length === 0 || this.#credit <= 0) return;
    const source = this.#chunks[0];
    const count = Math.min(source.byteLength, this.#credit);
    const bytes = source.subarray(0, count);
    if (count === source.byteLength) this.#chunks.shift();
    else this.#chunks[0] = source.subarray(count);
    this.#queued -= count;
    this.#credit -= count;
    this.#writing = true;
    this.#notify();
    this.#terminal.write(bytes, () => {
      this.#credit += bytes.byteLength;
      this.#writing = false;
      this.#notify();
      this.#pump();
    });
  }

  #notify() { this.#onChange?.({ queuedBytes: this.#queued, credit: this.#credit }); }
}

class Session {
  #bridge;
  #vm = null;
  #config = null;
  #state = "cold";
  #generation = 0;
  #output;
  #nativeInput = false;
  #backpressureStop = false;

  constructor(bridge) {
    this.#bridge = bridge;
    this.#output = new OutputQueue(terminal, {
      maxBytes: DEFAULT_MAX_OUTPUT_BYTES,
      credit: DEFAULT_WRITE_CREDIT,
      onChange: (metrics) => {
        elements.queue.textContent = `Queue ${formatBytes(metrics.queuedBytes)}`;
        if (this.#backpressureStop && metrics.queuedBytes === 0 && metrics.credit > 0) {
          void this.#resumeAfterBackpressure();
        }
      },
      onOverflow: (size) => this.#handleOutputOverflow(size),
    });
  }

  get state() { return this.#state; }
  get acceptsBrowserInput() { return !this.#nativeInput; }

  async bootstrap(config = {}) {
    this.#config = normalizeConfig(config);
    this.#nativeInput = this.#bridge.connected || config.inputMode === "native";
    await this.create(this.#config);
    if (config.autoStart !== false) {
      await this.start();
      try {
        await this.#bridge.request("start", { state: this.#state });
      } catch (error) {
        this.#handleError(error);
        throw error;
      }
    }
  }

  async create(config = this.#config) {
    if (this.#state !== "cold" && this.#state !== "destroyed" && this.#state !== "failed") {
      return this.#state;
    }
    if (!config) throw new Error("VM bootstrap configuration is missing");
    this.#config = normalizeConfig(config);
    this.#setState("loading");
    setOverlay("Loading", "Preparing the guest machine.");
    const generation = ++this.#generation;
    try {
      const vmOptions = buildVMOptions(this.#config, {
        console: (data) => this.#output.push(data),
        networkTransmit: () => {},
        downloadProgress: (progress) => this.#reportProgress(progress),
        stop: (detail) => this.#handleStop(detail),
        error: (error) => this.#handleError(error),
        diskError: (detail) => this.#bridge.event("disk-error", detail),
      });
      const vm = await RV64.create(vmOptions);
      if (generation !== this.#generation) {
        await vm.destroy();
        return this.#state;
      }
      this.#vm = vm;
      this.#setState("stopped");
      hideOverlay();
      this.#bridge.event("ready", {
        state: this.#state,
        protocol: PROTOCOL_VERSION,
        cols: terminal.cols,
        rows: terminal.rows,
      });
      this.#reportResize();
      return this.#state;
    } catch (error) {
      this.#setState("failed", error);
      throw error;
    }
  }

  async start() {
    this.#requireVM("start");
    if (this.#state === "running") return this.#state;
    if (!["stopped", "suspended"].includes(this.#state)) throw new Error(`cannot start from ${this.#state}`);
    this.#setState("starting");
    await this.#vm.start();
    this.#setState("running");
    return this.#state;
  }

  async stop() {
    if (!this.#vm || ["stopped", "suspended", "destroyed", "cold", "failed"].includes(this.#state)) return this.#state;
    this.#setState("stopping");
    await this.#vm.stop();
    this.#setState("stopped");
    return this.#state;
  }

  async reset() {
    this.#requireVM("reset");
    const restart = this.#state === "running";
    this.#setState("loading");
    await this.#vm.reset();
    this.#setState(restart ? "running" : "stopped");
    this.#output.clear();
    return this.#state;
  }

  async quiesce() {
    if (!this.#vm || this.#state === "cold" || this.#state === "destroyed") return this.#state;
    if (this.#state === "suspended") return this.#state;
    this.#setState("quiescing");
    if (typeof this.#vm.quiesce === "function") await this.#vm.quiesce();
    else {
      await this.#vm.stop();
      // Current RV64 exposes stop only. The native host owns the durable disk
      // flush and network shutdown until the Worker lifecycle API is extended.
      this.#bridge.event("state", { state: "suspended", compatibility: "stop" });
    }
    this.#setState("suspended");
    return this.#state;
  }

  async resume() {
    this.#requireVM("resume");
    if (this.#state !== "suspended") return this.#state;
    if (typeof this.#vm.resume === "function") await this.#vm.resume();
    else await this.#vm.start();
    this.#setState("running");
    return this.#state;
  }

  async destroy() {
    ++this.#generation;
    if (this.#vm) await this.#vm.destroy();
    this.#vm = null;
    this.#output.clear();
    this.#setState("destroyed");
    showOverlay("Session ended", "The native host can create a new session.");
    return this.#state;
  }

  async focusInput() {
    terminal.focus();
    this.#bridge.event("focus-input", { focused: true });
    return { focused: true };
  }

  requestNativeInputFocus() {
    terminal.focus();
    if (!this.#bridge.connected) return false;
    return this.#bridge.post(makeMessage("request", {
      id: `focus-${Date.now()}`,
      op: "focus-input",
      payload: { state: this.#state },
    }));
  }

  async resize(cols, rows) {
    const nextCols = clampInteger(cols, 2, 500, terminal.cols);
    const nextRows = clampInteger(rows, 1, 300, terminal.rows);
    terminal.resize(nextCols, nextRows);
    this.#reportResize();
    return { cols: terminal.cols, rows: terminal.rows };
  }

  reportResize() { this.#reportResize(); }

  reportError(error) { this.#handleError(error); }

  input(message) {
    if (!this.#vm || ["cold", "destroyed", "failed"].includes(this.#state)) return;
    const bytes = inputBytes(message);
    if (bytes.length) this.#vm.console.send(bytes);
  }

  #requireVM(operation) {
    if (!this.#vm) throw new Error(`cannot ${operation} before create`);
  }

  #setState(state, error = null) {
    this.#state = state;
    elements.state.textContent = stateLabel(state);
    elements.state.dataset.kind = state === "failed" ? "error" : ["cold", "destroyed"].includes(state) ? "muted" : "";
    elements.connection.textContent = this.#bridge.connected ? "Host connected" : "Development host";
    updateActionAvailability(state);
    this.#bridge.event("state", {
      state,
      error: error ? serializedError(error) : undefined,
      terminal: { queuedBytes: this.#output.queuedBytes, credit: this.#output.credit },
    });
  }

  #handleStop(detail) {
    if (this.#state === "running" && detail?.reason === "powered-off") this.#setState("stopped");
    if (detail?.reason === "error") this.#setState("failed", new Error("guest stopped with an error"));
  }

  #handleError(error) {
    this.#setState("failed", error);
    this.#bridge.event("error", serializedError(error));
    showOverlay("Guest error", serializedError(error).message);
  }

  #handleOutputOverflow(size) {
    this.#bridge.event("error", { name: "OutputBackpressure", message: `terminal output queue limit exceeded (${size} bytes)` });
    if (!this.#backpressureStop && this.#vm && this.#state === "running") {
      this.#backpressureStop = true;
      void this.#vm.stop().catch((error) => this.#handleError(error));
      this.#setState("stopped");
    }
  }

  async #resumeAfterBackpressure() {
    if (!this.#backpressureStop) return;
    this.#backpressureStop = false;
    if (this.#state === "stopped") {
      try { await this.start(); } catch (error) { this.#handleError(error); }
    }
  }

  #reportProgress(progress) {
    const image = progress?.image ?? "guest";
    const loaded = Number(progress?.loaded ?? 0);
    const total = Number(progress?.total ?? 0);
    const detail = total ? `${image} ${Math.round(loaded / total * 100)}%` : `Loading ${image}`;
    setOverlay("Loading", detail);
  }

  #reportResize() {
    const payload = { cols: terminal.cols, rows: terminal.rows };
    elements.dimensions.textContent = `${payload.cols} x ${payload.rows}`;
    this.#bridge.event("terminal-resize", payload);
  }
}

function stateLabel(state) {
  return {
    cold: "Waiting for host", loading: "Loading", starting: "Starting", running: "Running",
    stopping: "Stopping", stopped: "Stopped", quiescing: "Pausing", suspended: "Paused",
    failed: "Failed", destroyed: "Ended",
  }[state] ?? state;
}

function formatBytes(value) {
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KiB`;
  return `${(value / 1024 / 1024).toFixed(1)} MiB`;
}

function clampInteger(value, min, max, fallback) {
  const number = Number(value);
  return Number.isInteger(number) ? Math.min(max, Math.max(min, number)) : fallback;
}

function setOverlay(title, detail) {
  elements.loading.hidden = false;
  elements.loadingTitle.textContent = title;
  elements.loadingDetail.textContent = detail;
}

function hideOverlay() { elements.loading.hidden = true; }
function showOverlay(title, detail) { setOverlay(title, detail); }

function normalizeConfig(config) {
  if (!config || typeof config !== "object") throw new TypeError("bootstrap configuration must be an object");
  const source = { ...config };
  globalThis.LISH_DIAGNOSTICS = source.diagnostics === true;
  const boot = { ...(source.boot ?? {}) };
  boot.mode ??= source.bootMode ?? "linux-direct";
  boot.kernel ??= source.kernel ?? source.kernelURL ?? "../images/alpine/Image";
  boot.cmdline ??= source.cmdline ?? "console=ttyS0 root=/dev/vda rw init=/rv64-init";
  const configuredDisk = boot.disk ?? source.disk ?? source.diskURL;
  boot.disk = configuredDisk ?? "../images/alpine/alpine.ext4";
  if (typeof boot.disk === "string" && (source.diskSize !== undefined || source.diskURL !== undefined || source.diskMode === "native")) {
    boot.disk = {
      mode: "native",
      url: boot.disk,
      ...(source.diskSize === undefined ? {} : { size: Number(source.diskSize) }),
    };
  } else if (boot.disk && typeof boot.disk === "object" && boot.disk.mode === "native") {
    boot.disk = {
      ...boot.disk,
      url: new URL(boot.disk.url, document.baseURI).href,
    };
  }
  for (const key of ["firmware", "kernel", "initrd"]) {
    if (typeof boot[key] === "string") boot[key] = imageURL(boot[key]);
  }
  if (typeof boot.disk === "string") boot.disk = imageURL(boot.disk);
  source.boot = boot;
  source.wasm ??= source.wasmURL ?? "../rv64_wasm.wasm";
  if (typeof source.wasm === "string") source.wasm = imageURL(source.wasm);
  source.memoryMB = numberOr(source.memoryMB, 512);
  source.network ??= source.networkURL ? {
    mode: "wsproxy",
    url: source.networkURL,
    ...(source.networkProtocols ? { protocols: source.networkProtocols } : {}),
  } : { mode: "none" };
  return source;
}

function imageURL(value) {
  return { url: new URL(value, document.baseURI).href };
}

function buildVMOptions(config, events) {
  const network = { ...(config.network ?? { mode: "none" }) };
  if (network.mode === "wsproxy" && !network.protocols && config.capability) {
    network.protocols = ["lish.raw-ethernet.v1", config.capability];
  }
  const execution = { mode: "worker" };
  if (config.workerURL) execution.workerURL = config.workerURL;
  if (config.statisticsIntervalMs !== undefined) execution.statisticsIntervalMs = config.statisticsIntervalMs;
  return {
    wasm: config.wasm,
    memoryMB: config.memoryMB,
    boot: config.boot,
    network,
    execution,
    events,
  };
}

function inputBytes(message) {
  if (message && typeof message === "object") {
    if (message.bytes !== undefined) return asBytes(message.bytes).slice();
    if (message.data !== undefined && message.kind !== "key") return asBytes(message.data).slice();
    if (message.text !== undefined && message.kind !== "key") return asBytes(normalizeText(message.text));
    if (message.value !== undefined && message.kind !== "key") return asBytes(normalizeText(message.value));
    if (message.kind === "key" || message.key !== undefined) return keyBytes(message);
  }
  return asBytes(normalizeText(message));
}

function normalizeText(text) {
  return String(text ?? "").replace(/\r\n/g, "\r");
}

function pasteText(text) {
  const normalized = String(text ?? "").replace(/\r?\n/g, "\r");
  if (!normalized) return;
  const input = terminal.modes.bracketedPasteMode
    ? `\x1b[200~${normalized}\x1b[201~`
    : normalized;
  session.input({ data: input });
}

function keyBytes(message) {
  const key = String(message.key ?? message.data ?? "").toLowerCase();
  const arrows = { up: "A", down: "B", right: "C", left: "D" };
  if (arrows[key]) {
    const prefix = terminal.modes.applicationCursorKeysMode ? "O" : "[";
    return asBytes(`\x1b${prefix}${arrows[key]}`);
  }
  const values = {
    enter: "\r", return: "\r", backspace: "\x7f", delete: "\x1b[3~", tab: "\t",
    escape: "\x1b", esc: "\x1b", home: "\x1b[H", end: "\x1b[F", pageup: "\x1b[5~", pagedown: "\x1b[6~",
  };
  if (values[key]) return asBytes(values[key]);
  const text = String(message.text ?? message.data ?? "");
  if (message.ctrl && text.length === 1) return asBytes(Uint8Array.of(controlCode(text)));
  return asBytes(text);
}

function controlCode(value) {
  const code = value.toUpperCase().charCodeAt(0);
  if (code === 0x3f) return 0x7f;
  if (code >= 0x40 && code <= 0x5f) return code & 0x1f;
  return code & 0x1f;
}

function reportToNative(event, payload) { bridge.event(event, payload); }

function updateActionAvailability(state) {
  const enabled = {
    start: state === "stopped" || state === "suspended",
    stop: state === "running" || state === "starting",
    reset: ["running", "stopped", "suspended"].includes(state),
  };
  for (const button of document.querySelectorAll("button[data-action]")) {
    const action = button.dataset.action;
    if (action !== "clear") button.disabled = !enabled[action];
  }
}

const bridge = new NativeBridge();
const session = new Session(bridge);
updateActionAvailability(session.state);

// The AppKit shell uses the same low-rate bridge for input and control. Keep
// these aliases public so the native side can call them with bound arguments.
globalThis.lishNativeInput = (bytes) => session.input({ bytes });
globalThis.lishNativePaste = pasteText;
globalThis.lishNativeControlResult = (message) => bridge.receive(message);
globalThis.lishCopySelection = () => terminal.hasSelection() ? terminal.getSelection() : null;
globalThis.lishSelectAll = () => terminal.selectAll();

function handleIncoming(message) {
  if (bridge.receive(message)) return Promise.resolve(null);
  if (!message || typeof message !== "object" || Number(message.version) !== PROTOCOL_VERSION) {
    return Promise.reject(new Error("invalid Lish control message"));
  }
  if (message.type !== "request") return Promise.resolve(null);
  const id = message.id;
  const op = message.op;
  const result = dispatch(op, message.payload);
  void result.then(
    (payload) => bridge.post(controlResponse(id, op, true, payload)),
    (error) => bridge.post(controlResponse(id, op, false, error)),
  );
  return result;
}

async function dispatch(op, payload) {
  switch (op) {
    case "create": return session.create(payload);
    case "start": return session.start();
    case "stop": return session.stop();
    case "reset": return session.reset();
    case "quiesce": return session.quiesce();
    case "resume": return session.resume();
    case "destroy": return session.destroy();
    case "focus-input": return session.focusInput();
    case "console-resize": return session.resize(payload?.cols, payload?.rows);
    default: throw new Error(`unknown Lish operation: ${op}`);
  }
}

function receiveInput(message) { session.input(message); }

globalThis.lishBootstrap = (config) => session.bootstrap(config);
globalThis.lishControl = handleIncoming;
globalThis.lishControlRequest = (message) => handleIncoming(message);
globalThis.lishInput = receiveInput;
globalThis.lishTerminalInput = receiveInput;
globalThis.lishProtocol = Object.freeze({ version: PROTOCOL_VERSION, receive: handleIncoming });
globalThis.addEventListener?.("lish-control", (event) => handleIncoming(event.detail ?? event.data));
reportToNative("page-ready", { protocol: PROTOCOL_VERSION });

terminal.attachCustomKeyEventHandler(() => session.acceptsBrowserInput);
terminal.onData((data) => {
  if (session.acceptsBrowserInput) session.input({ data });
});
let reportedSelection = false;
terminal.onSelectionChange(() => {
  const hasSelection = terminal.hasSelection();
  if (hasSelection === reportedSelection) return;
  reportedSelection = hasSelection;
  reportToNative("selection-change", { hasSelection });
});
elements.terminal.addEventListener("focusin", () => reportToNative("focus-input", { focused: true }));
terminal.onResize(({ cols, rows }) => {
  elements.dimensions.textContent = `${cols} x ${rows}`;
  if (session.state !== "cold") reportToNative("terminal-resize", { cols, rows });
});
elements.terminal.addEventListener("pointerdown", () => {
  session.requestNativeInputFocus();
});

for (const button of document.querySelectorAll("button[data-action]")) {
  button.addEventListener("click", () => {
    const op = button.dataset.action;
    if (op === "clear") { terminal.clear(); return; }
    void dispatch(op).catch((error) => session.reportError(error));
  });
}

const resizeObserver = new ResizeObserver(() => {
  fitAddon.fit();
  session.reportResize();
});
resizeObserver.observe(elements.terminal);
fitAddon.fit();

const parameters = new URLSearchParams(globalThis.location?.search ?? "");
const devConfig = globalThis.__LISH_DEV_BOOTSTRAP__;
if (devConfig && parameters.get(DEV_QUERY) === "1") {
  void session.bootstrap({ ...devConfig, inputMode: "browser" }).catch((error) => {
    setOverlay("Boot failed", serializedError(error).message);
  });
} else if (bridge.connected) {
  elements.connection.textContent = "Host connected";
  setOverlay("Waiting for host", "The native session will start this machine.");
} else {
  elements.connection.textContent = "Development host";
  elements.loadingDetail.textContent = "Use an explicit development bootstrap to run this page.";
}

export { PROTOCOL_VERSION, session, handleIncoming };
