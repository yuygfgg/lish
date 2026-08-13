#!/usr/bin/env node
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { RV64 } from "../web/rv64.js";
import { idleKernel } from "./synthetic-kernels.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const wasm = await readFile(
  join(root, "target/wasm32-unknown-unknown/release/rv64_wasm.wasm"),
);
const events = [];
const vm = await RV64.create({
  wasm,
  memoryMB: 8,
  boot: {
    mode: "linux-direct",
    kernel: idleKernel(),
  },
  events: {
    ready: () => events.push("ready"),
    start: () => events.push("start"),
    stop: ({ reason }) => events.push(`stop:${reason}`),
  },
});

assert.equal(vm.running, false);
assert.equal(vm.network.mode, "none");
assert.deepEqual(events, ["ready"]);
assert.equal("ex" in vm, false);
for (const removed of [
  "bootLinux",
  "bootVirtLinux",
  "runSystem",
  "runVirtSystem",
  "consoleInput",
  "virtConsoleInput",
]) {
  assert.equal(removed in vm, false, `${removed} must not be public`);
}

const unsubscribe = vm.on("start", () => events.push("second-start"));
await vm.start();
assert.equal(vm.running, true);
while (vm.instructions === 0n) await new Promise((resolve) => setImmediate(resolve));
await vm.stop();
assert.deepEqual(events, ["ready", "start", "second-start", "stop:requested"]);
assert.ok(vm.instructions > 0n);

unsubscribe();
await vm.reset();
assert.equal(vm.instructions, 0n);
await vm.start();
while (vm.instructions === 0n) await new Promise((resolve) => setImmediate(resolve));
await vm.stop();
assert.equal(events.filter((event) => event === "second-start").length, 1);

await vm.destroy();
await vm.destroy();
assert.throws(() => vm.instructions, /destroyed/);

const direct = await RV64.create({
  wasm,
  memoryMB: 8,
  boot: {
    mode: "linux-direct",
    kernel: new Uint8Array([0x73, 0x00, 0x10, 0x00]),
  },
});
assert.equal(direct.running, false);
assert.equal(direct.instructions, 0n);
assert.equal(direct.network.mode, "none");
assert.throws(() => direct.network.receive(new Uint8Array(14)), /external mode/);
await direct.destroy();

await assert.rejects(
  RV64.create({
    wasm,
    boot: {
      mode: "linux-direct",
      kernel: new Uint8Array([0x73, 0x00, 0x10, 0x00]),
    },
    network: { mode: "fetch" },
  }),
  /unknown network mode: fetch/,
);

const external = await RV64.create({
  wasm,
  memoryMB: 8,
  boot: {
    mode: "linux-direct",
    kernel: new Uint8Array([0x73, 0x00, 0x10, 0x00]),
  },
  network: { mode: "external", mac: new Uint8Array([2, 0, 0, 0, 0, 2]) },
});
assert.equal(external.network.mode, "external");
external.network.receive(new Uint8Array(14));
await external.destroy();

const RealWebSocket = globalThis.WebSocket;
class TestWebSocket {
  static OPEN = 1;
  readyState = 0;
  constructor(url, protocols) {
    this.url = url;
    this.protocols = protocols;
  }
  close() { this.readyState = 3; }
  send() {}
}
globalThis.WebSocket = TestWebSocket;
for (const network of [{ mode: "wsproxy", url: "wss://relay.example/" }]) {
  const networkVM = await RV64.create({
    wasm,
    memoryMB: 8,
    boot: { mode: "linux-direct", kernel: new Uint8Array([0x73, 0, 0x10, 0]) },
    network,
  });
  assert.equal(networkVM.network.mode, network.mode);
  await networkVM.destroy();
}
globalThis.WebSocket = RealWebSocket;

await assert.rejects(
  RV64.create({
    wasm,
    boot: { mode: "linux-direct", kernel: new Uint8Array(4) },
    network: { mode: "wisp" },
  }),
  /unknown network mode: wisp/,
);

await assert.rejects(
  RV64.create({
    wasm,
    boot: { mode: "bare-metal", image: new Uint8Array(4) },
  }),
  /unknown boot mode: bare-metal/,
);
console.log("PASS stable public API lifecycle");
