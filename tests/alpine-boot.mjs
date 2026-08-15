#!/usr/bin/env node
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { RV64 } from "../web/rv64.js";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const wasmPath = join(root, "target/wasm32-unknown-unknown/release/rv64_wasm.wasm");
const kernelPath = process.env.RV64_MODERN_KERNEL || join(root, "web/images/alpine/Image");
const diskPath = join(root, "web/images/alpine/alpine.ext4");
if (![wasmPath, kernelPath, diskPath].every(existsSync)) {
  console.error("FAIL Alpine boot: missing Wasm, kernel, or disk; run web/prepare-images.sh first");
  process.exit(2);
}

let output = "";
let observedError;
const decoder = new TextDecoder();
const vm = await RV64.create({
  wasm: await readFile(wasmPath),
  memoryMB: 512,
  boot: {
    mode: "linux-direct",
    kernel: await readFile(kernelPath),
    disk: await readFile(diskPath),
    cmdline: "console=ttyS0 root=/dev/vda rw init=/rv64-init",
  },
  network: { mode: "none" },
  events: {
    console(bytes) {
      output += decoder.decode(bytes, { stream: true });
      if (process.env.RV64_BOOT_TRACE) process.stdout.write(bytes);
    },
    error(error) { observedError = error; },
  },
});

assert.equal(vm.network.mode, "none");
await vm.start();
async function waitForOutput(marker, timeoutMs) {
  const deadline = performance.now() + timeoutMs;
  while (vm.running && !output.includes(marker) && performance.now() < deadline) {
    await new Promise((resolveImmediate) => setImmediate(resolveImmediate));
  }
  assert.ifError(observedError);
  assert.ok(output.includes(marker), `timed out waiting for ${marker}`);
}

try {
  await waitForOutput("ALPINE_READY", 240_000);
  await waitForOutput("\x1b[6n", 30_000);
  vm.console.send("\x1b[1;1R");

  vm.console.send(
    "sh -c 'prefix=LISH_SIGINT_; trap \"echo ${prefix}OK; exit 0\" INT; " +
      "echo ${prefix}ARMED; while :; do :; done'\r",
  );
  await waitForOutput("LISH_SIGINT_ARMED", 30_000);
  vm.console.send(Uint8Array.of(0x03));
  await waitForOutput("LISH_SIGINT_OK", 30_000);

  assert.match(output, /Linux version/);
  assert.doesNotMatch(output, /unexpected end of file/);
} finally {
  await vm.stop();
  await vm.destroy();
}
console.log("PASS Alpine direct boot with networking disabled");
