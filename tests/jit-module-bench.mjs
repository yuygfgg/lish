import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { RV64Debug, Stop } from "../web/rv64.js";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const wasmPath = join(root, "target/wasm32-unknown-unknown/release/rv64_wasm.wasm");
const benchmarkPath = join(root, "target/guest-benchmarks/rv64-jit-bench");
if (![wasmPath, benchmarkPath].every(existsSync)) {
  console.log("SKIP JIT module benchmark (build Wasm and guest benchmarks first)");
  process.exit(0);
}

const vm = await RV64Debug.create(await readFile(wasmPath), {
  maxModules: 2,
  maxSlots: 2,
  maxBytes: 8 * 1024 * 1024,
  growSlots: 1,
});
let output = "";
vm.onWrite = (_fd, bytes) => {
  output += new TextDecoder().decode(bytes);
};
assert.equal(
  vm.loadElf(
    new Uint8Array(await readFile(benchmarkPath)),
    ["rv64-jit-bench", "32", "4", "4096"],
    64,
  ),
  true,
);

let stop = Stop.YIELD;
for (let slice = 0; slice < 20_000 && stop === Stop.YIELD; slice++) {
  const before = vm.userInsnCount();
  stop = vm.runUser(100_000n);
  assert.ok(
    stop !== Stop.YIELD || vm.userInsnCount() > before,
    "benchmark yielded without retiring an instruction",
  );
}
const metrics = vm.jitMetrics();
assert.equal(stop, Stop.EXITED, "benchmark did not exit");
assert.equal(vm.userExitCode(), 0);
assert.match(output, /JIT_BENCH done/);
assert.ok(metrics.retiredSlots > 0, "self-modifying code did not retire JIT slots");
assert.equal(metrics.liveSlots, metrics.rustLiveSlots);
assert.ok(metrics.tableHighWater <= metrics.limits.maxSlots);
assert.ok(metrics.capacityRejects > 0, "bounded store did not exercise capacity eviction");
assert.ok(metrics.evictedSlots > 0, "capacity pressure did not evict a cold owner");

console.log(
  `PASS JIT module churn benchmark ` +
  `high-water=${metrics.tableHighWater} retired=${metrics.retiredSlots} ` +
  `evicted=${metrics.evictedSlots}`,
);
