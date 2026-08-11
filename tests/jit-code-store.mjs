import assert from "node:assert/strict";
import { JitCodeStore } from "../web/rv64.js";

function leb(value) {
  const bytes = [];
  do {
    let byte = value & 0x7f;
    value >>>= 7;
    if (value) byte |= 0x80;
    bytes.push(byte);
  } while (value);
  return bytes;
}

function section(id, payload) {
  return [id, ...leb(payload.length), ...payload];
}

function functionModule(names) {
  const encoder = new TextEncoder();
  const exports = [];
  for (let i = 0; i < names.length; i++) {
    const name = encoder.encode(names[i]);
    exports.push(...leb(name.length), ...name, 0, ...leb(i));
  }
  const bodies = names.flatMap(() => [2, 0, 0x0b]);
  return new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array([
    0, 0x61, 0x73, 0x6d, 1, 0, 0, 0,
    ...section(1, [1, 0x60, 0, 0]),
    ...section(3, [...leb(names.length), ...names.flatMap(() => [0])]),
    ...section(7, [...leb(names.length), ...exports]),
    ...section(10, [...leb(names.length), ...bodies]),
  ])));
}

const table = new WebAssembly.Table({ element: "anyfunc", initial: 4 });
const store = new JitCodeStore(table, {
  maxModules: 4,
  maxSlots: 4,
  maxBytes: 100,
  growSlots: 2,
});

const one = functionModule(["run"]);
const batch = functionModule(["r0", "r1"]);
const first = store.register(one, ["run"], 10);
const batchBase = store.register(batch, ["r0", "r1"], 20);
assert.equal(first, 4);
assert.equal(batchBase, 5);
assert.equal(store.snapshot().liveModules, 2);
assert.equal(store.snapshot().liveSlots, 3);

store.retire(batchBase);
assert.equal(typeof table.get(batchBase), "function", "retirement must be deferred");
store.flushRetired();
assert.equal(table.get(batchBase), null);

const reused = store.register(one, ["run"], 10);
assert.equal(reused, batchBase, "the first free table slot must be reused");
assert.equal(store.snapshot().tableHighWater, 3);

assert.equal(store.canRegister(2, 1), false, "slot limit must reject overflow");
assert.equal(store.canRegister(1, 81), false, "byte limit must reject overflow");

store.retire(first);
store.retire(batchBase + 1, 1);
store.retire(reused, 1);
store.flushRetired();
assert.deepEqual(
  { modules: store.snapshot().liveModules, slots: store.snapshot().liveSlots },
  { modules: 0, slots: 0 },
);
assert.equal(store.snapshot().evictedSlots, 2);
assert.equal(store.snapshot().evictedModules, 2);

const generation = store.generation;
store.register(batch, ["r0", "r1"], 20);
store.clear();
assert.equal(store.generation, generation + 1);
assert.equal(store.snapshot().liveModules, 0);
assert.equal(store.snapshot().liveSlots, 0);
assert.equal(store.snapshot().tableHighWater, 3);

console.log("PASS bounded JIT code store ownership and slot reuse");
