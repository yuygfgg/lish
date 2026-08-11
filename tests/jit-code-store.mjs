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

const reservationTable = new WebAssembly.Table({
  element: "anyfunc",
  initial: 0,
});
const reservationStore = new JitCodeStore(reservationTable, {
  maxModules: 2,
  maxSlots: 4,
  maxBytes: 40,
  growSlots: 2,
});
const pendingBatch = reservationStore.reserve(2, 20);
assert.notEqual(pendingBatch, null);
assert.deepEqual(
  {
    modules: reservationStore.snapshot().pendingModules,
    slots: reservationStore.snapshot().pendingSlots,
    bytes: reservationStore.snapshot().pendingBytes,
  },
  { modules: 1, slots: 2, bytes: 20 },
);
assert.equal(
  reservationStore.reserve(3, 1),
  null,
  "pending slots must count against the hard limit",
);
assert.equal(
  reservationStore.reserve(1, 21),
  null,
  "pending bytes must count against the hard limit",
);

const pendingOne = reservationStore.reserve(1, 10);
assert.notEqual(pendingOne, null);
assert.equal(
  reservationStore.canRegister(1, 1),
  false,
  "pending modules must count against the hard limit",
);
assert.equal(reservationStore.releaseReservation(pendingOne), true);

const reservedBase = reservationStore.registerReserved(
  pendingBatch,
  batch,
  ["r0", "r1"],
);
assert.equal(reservedBase, 0);
assert.deepEqual(
  {
    liveModules: reservationStore.snapshot().liveModules,
    liveSlots: reservationStore.snapshot().liveSlots,
    liveBytes: reservationStore.snapshot().liveBytes,
    pendingModules: reservationStore.snapshot().pendingModules,
    pendingSlots: reservationStore.snapshot().pendingSlots,
    pendingBytes: reservationStore.snapshot().pendingBytes,
  },
  {
    liveModules: 1,
    liveSlots: 2,
    liveBytes: 20,
    pendingModules: 0,
    pendingSlots: 0,
    pendingBytes: 0,
  },
  "reservation consumption must be an atomic pending-to-live transition",
);
assert.equal(reservationStore.releaseReservation(pendingBatch), false);
assert.equal(reservationStore.snapshot().peakModules, 1);
assert.equal(reservationStore.snapshot().peakSlots, 2);
assert.equal(reservationStore.snapshot().peakBytes, 20);
assert.equal(reservationStore.snapshot().peakPendingModules, 2);
assert.equal(reservationStore.snapshot().peakPendingSlots, 3);
assert.equal(reservationStore.snapshot().peakPendingBytes, 30);
assert.equal(reservationStore.snapshot().peakCommittedModules, 2);
assert.equal(reservationStore.snapshot().peakCommittedSlots, 3);
assert.equal(reservationStore.snapshot().peakCommittedBytes, 30);

const staleTable = new WebAssembly.Table({ element: "anyfunc", initial: 0 });
const staleStore = new JitCodeStore(staleTable, {
  maxModules: 2,
  maxSlots: 4,
  maxBytes: 40,
  growSlots: 2,
});
const stale = staleStore.reserve(1, 10);
staleStore.clear();
assert.equal(
  staleStore.registerReserved(stale, one, ["run"]),
  -1,
  "the store must reject a reservation from an earlier generation",
);
assert.equal(staleStore.snapshot().pendingModules, 0);
assert.equal(staleStore.snapshot().freeSlots, 1);

const failed = staleStore.reserve(2, 20);
assert.throws(
  () => staleStore.registerReserved(
    failed,
    { exports: { r0: batch.exports.r0, r1: () => {} } },
    ["r0", "r1"],
  ),
  TypeError,
  "a partial table installation must roll back its complete reservation",
);
assert.equal(staleStore.snapshot().pendingModules, 0);
assert.equal(staleStore.snapshot().liveModules, 0);
const afterFailure = staleStore.reserve(3, 20);
assert.equal(
  afterFailure.base,
  0,
  "a failed install did not coalesce and reuse its complete table run",
);
assert.equal(staleStore.releaseReservation(afterFailure), true);

const terminal = staleStore.reserve(1, 10);
assert.notEqual(terminal, null);
staleStore.destroy();
assert.equal(staleStore.snapshot().destroyed, true);
assert.equal(staleStore.snapshot().pendingModules, 0);
assert.equal(staleStore.reserve(1, 1), null, "destroyed stores accepted new work");
staleStore.destroy();

console.log("PASS bounded JIT code store ownership and slot reuse");
