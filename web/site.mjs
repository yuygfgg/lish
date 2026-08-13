import { RV64 } from "./rv64.js?v=3";
import { Terminal } from "./vendor/xterm/xterm.js";
import { FitAddon } from "./vendor/xterm/addon-fit.js";

export const PRESETS = Object.freeze({
  alpine: {
    label: "Alpine Linux",
    ramMB: 512,
    local: ["images/alpine/Image", "images/alpine/alpine.ext4"],
  },
});

const terminalElement = document.querySelector("#terminal");
const terminal = new Terminal({
  convertEol: true,
  cursorBlink: true,
  fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
  fontSize: 14,
  lineHeight: 1.2,
  scrollback: 5000,
  theme: {
    background: "#030504",
    foreground: "#b9f6ce",
    cursor: "#6ee7a8",
    selectionBackground: "#355b48",
  },
});
const fitAddon = new FitAddon();
terminal.loadAddon(fitAddon);
terminal.open(terminalElement);
fitAddon.fit();
terminal.write("Press Boot Alpine Linux to start.\r\n");
new ResizeObserver(() => fitAddon.fit()).observe(terminalElement);
const status = document.querySelector("#status");
const networkStatus = document.querySelector("#network-status");
const boot = document.querySelector("#boot");
const title = document.querySelector("#terminal-title");
const decoder = new TextDecoder();
const encoder = new TextEncoder();
let active = null;
let generation = 0;
const requestedExecution = new URLSearchParams(location.search).get("execution");
const executionMode = requestedExecution === "local" ? "local" : "worker";
const pageParameters = new URLSearchParams(location.search);

function networkConfiguration() {
  const url = pageParameters.get("network");
  if (!url) return { mode: "none" };
  const parsed = new URL(url, location.href);
  if (parsed.protocol !== "ws:" && parsed.protocol !== "wss:") {
    throw new Error("network must use ws:// or wss:// raw Ethernet transport");
  }
  const protocols = ["lish.raw-ethernet.v1"];
  const capability = pageParameters.get("capability");
  if (!capability) throw new Error("raw Ethernet networking requires capability");
  protocols.push(capability);
  return { mode: "wsproxy", url: parsed.href, protocols };
}

function cpuStatus(text) {
  status.textContent = `${text} · ${executionMode}`;
}
cpuStatus("Ready");

function write(data) {
  terminal.write(typeof data === "string" ? data : decoder.decode(data, { stream: true }));
}

async function downloadAsset(candidate, progress) {
  const response = await fetch(candidate);
  if (!response.ok) throw new Error(`${response.status} ${response.statusText}`);
  const total = response.headers.has("content-encoding")
    ? 0
    : Number(response.headers.get("content-length")) || 0;
  if (!response.body) return new Uint8Array(await response.arrayBuffer());
  const reader = response.body.getReader();
  const chunks = [];
  let loaded = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    loaded += value.length;
    progress(loaded, total);
  }
  const result = new Uint8Array(loaded);
  let offset = 0;
  for (const chunk of chunks) { result.set(chunk, offset); offset += chunk.length; }
  return result;
}

async function fetchAsset(local, progress) {
  const candidates = Array.isArray(local) ? local : [local];
  let lastError;
  for (const candidate of candidates) {
    try {
      return await downloadAsset(candidate, progress);
    } catch (error) {
      lastError = new Error(`${candidate}: ${error.message}`);
    }
  }
  throw lastError ?? new Error("asset has no source");
}

async function loadWasm() {
  return fetchAsset(
    ["./rv64_wasm.wasm", "../target/wasm32-unknown-unknown/release/rv64_wasm.wasm"],
    () => {},
  );
}

async function start(presetName) {
  const myGeneration = ++generation;
  const preset = PRESETS[presetName];
  boot.disabled = true;
  terminal.clear();
  networkStatus.textContent = "Ethernet idle";
  networkStatus.title = "";
  write(`[host] loading ${preset.label}…\n`);
  try {
    if (active) await active.destroy();
    active = null;
    const wasm = await loadWasm();
    const images = [];
    for (let i = 0; i < preset.local.length; i++) {
      const name = preset.local[i].split("/").at(-1);
      images.push(await fetchAsset(preset.local[i], (loaded, total) => {
        const amount = total ? `${(loaded / total * 100).toFixed(0)}%` : `${(loaded / 1048576).toFixed(1)} MiB`;
        cpuStatus(`Downloading ${name}: ${amount}`);
      }));
    }
    if (myGeneration !== generation) return;
    const bootConfig = {
      mode: "linux-direct",
      kernel: images[0],
      disk: images[1],
      cmdline: "console=ttyS0 root=/dev/vda rw init=/rv64-init",
    };
    const vm = await RV64.create({
      wasm,
      memoryMB: preset.ramMB,
      execution: { mode: executionMode },
      boot: bootConfig,
      network: networkConfiguration(),
      events: {
        console: (data) => write(data),
        networkTransmit: (frame) => {
          networkStatus.textContent = `Ethernet · ${frame.byteLength} bytes sent`;
        },
        stop: ({ reason }) => {
          if (reason === "powered-off") write("\n[host] guest powered off\n");
          cpuStatus(reason === "powered-off" ? "Powered off" : "Stopped");
        },
      },
    });
    active = vm;
    const started = performance.now();
    let lastStatus = 0;
    let lastInstructions = 0;
    const frame = () => {
      if (myGeneration !== generation || !active) return;
      if (!active.running) return;
      const now = performance.now();
      if (now - lastStatus > 500) {
        const insns = Number(active.instructions);
        const elapsed = lastStatus === 0 ? now - started : now - lastStatus;
        const delta = lastStatus === 0 ? insns : insns - lastInstructions;
        const rate = elapsed > 0 ? delta / elapsed / 1000 : 0;
        const jit = active.jitMetrics?.();
        const pending = jit?.rustPendingBuilds ?? 0;
        cpuStatus(
          `${(insns / 1e6).toFixed(0)} Minsns · ${rate.toFixed(1)} Minsn/s · JIT ${pending} pending`,
        );
        lastStatus = now;
        lastInstructions = insns;
      }
      setTimeout(frame, 500);
    };
    title.textContent = `${preset.label} console`;
    terminal.focus();
    await vm.start();
    frame();
  } catch (error) {
    write(`\n[host] unable to boot: ${error.message}\n\n`);
    write("Build the local assets before starting the page.\n");
    cpuStatus("Boot failed");
    console.error(error);
  } finally {
    boot.disabled = false;
  }
}

boot.addEventListener("click", () => start("alpine"));
document.querySelector("#clear").addEventListener("click", () => terminal.clear());
terminal.onData((data) => {
  if (active) active.console.send(encoder.encode(data));
});
