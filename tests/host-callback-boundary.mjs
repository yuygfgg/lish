import assert from "node:assert/strict";
import { RV64Debug } from "../web/rv64.js";

// This module is deliberately small. It exercises the host ABI that the
// full-system Wasm module uses today. It must not grow compatibility imports
// for removed user-mode or proxy transports.
const I32 = 0x7f;
const I64 = 0x7e;
const F64 = 0x7c;

function uleb(value) {
  const bytes = [];
  do {
    let byte = value & 0x7f;
    value = Math.floor(value / 128);
    if (value !== 0) byte |= 0x80;
    bytes.push(byte);
  } while (value !== 0);
  return bytes;
}

function sleb(value) {
  const bytes = [];
  let more = true;
  while (more) {
    let byte = value & 0x7f;
    value >>= 7;
    const sign = (byte & 0x40) !== 0;
    more = !((value === 0 && !sign) || (value === -1 && sign));
    if (more) byte |= 0x80;
    bytes.push(byte);
  }
  return bytes;
}

function vector(items) {
  return [...uleb(items.length), ...items.flat()];
}

function section(id, payload) {
  return [id, ...uleb(payload.length), ...payload];
}

function name(value) {
  const bytes = new TextEncoder().encode(value);
  return [...uleb(bytes.length), ...bytes];
}

function functionType(params, results) {
  return [0x60, ...vector(params), ...vector(results)];
}

function functionImport(field, typeIndex) {
  return [...name("env"), ...name(field), 0x00, ...uleb(typeIndex)];
}

function functionExport(field, functionIndex) {
  return [...name(field), 0x00, ...uleb(functionIndex)];
}

function functionBody(code) {
  const body = [0x00, ...code, 0x0b];
  return [...uleb(body.length), ...body];
}

const i32Const = (value) => [0x41, ...sleb(value)];
const i64Const = (value) => [0x42, ...sleb(value)];
const localGet = (index) => [0x20, ...uleb(index)];
const globalGet = (index) => [0x23, ...uleb(index)];
const globalSet = (index) => [0x24, ...uleb(index)];
const call = (index) => [0x10, ...uleb(index)];

function compiledBlockModule() {
  return new Uint8Array([
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
    ...section(1, vector([functionType([I32], [])])),
    ...section(3, vector([uleb(0)])),
    ...section(7, vector([functionExport("run", 0)])),
    ...section(10, vector([functionBody([])])),
  ]);
}

function boundaryModule(compiled) {
  const types = [
    functionType([I32, I32, I32], []), // host_write
    functionType([I32, I32], []), // host_net_send
    functionType([], [F64]), // clock imports
    functionType([I32, I32], []), // host_jit_retire
    functionType([I64, I32], []), // host_jit_register_async
    functionType([I64, I32, I32], []), // sys_jit_ready
    functionType([], []),
    functionType([I64], []),
    functionType([], [F64]),
    functionType([I32, I64, I32], [I64]),
    functionType([], [I32]),
    functionType([I32], []),
  ];
  const imports = [
    functionImport("host_write", 0),
    functionImport("host_net_send", 1),
    functionImport("host_now_ms", 2),
    functionImport("host_unix_ms", 2),
    functionImport("host_jit_retire", 3),
    functionImport("host_jit_register_async", 4),
  ];
  const functionNames = [
    "emit_write",
    "emit_net",
    "emit_both",
    "now",
    "unix",
    "trigger_async",
    "ready_status",
    "sys_jit_ready",
    "jit_out_ptr",
    "jit_out_len",
    "retire_slot",
    "chain_next",
    "jit_tlb_fill",
    "full_system_dispatch_abort",
  ];
  const functionTypes = [
    6, 6, 6, 8, 8, 7, 10, 5, 10, 10, 11, 6, 9, 6,
  ];
  const firstFunction = imports.length;
  const exports = functionNames.map((field, index) =>
    functionExport(field, firstFunction + index)
  );
  exports.push([...name("__indirect_function_table"), 0x01, 0x00]);
  exports.push([...name("memory"), 0x02, 0x00]);

  const write = [
    ...i32Const(1), ...i32Const(0), ...i32Const(4), ...call(0),
  ];
  const net = [...i32Const(0), ...i32Const(4), ...call(1)];
  const bodies = [
    write,
    net,
    [...write, ...net],
    [...call(2)],
    [...call(3)],
    [...localGet(0), ...i32Const(1), ...call(5)],
    [...globalGet(0)],
    [...localGet(1), ...globalSet(0)],
    [...i32Const(64)],
    [...i32Const(compiled.length)],
    [...localGet(0), ...i32Const(1), ...call(4)],
    [],
    [...i64Const(-1)],
    [],
  ].map(functionBody);
  const globals = [[I32, 0x01, ...i32Const(7), 0x0b]];
  const data = [
    [0x00, ...i32Const(0), 0x0b, ...uleb(4), 1, 2, 3, 4],
    [0x00, ...i32Const(64), 0x0b, ...uleb(compiled.length), ...compiled],
  ];
  return new Uint8Array([
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
    ...section(1, vector(types)),
    ...section(2, vector(imports)),
    ...section(3, vector(functionTypes.map((type) => uleb(type)))),
    ...section(4, [0x01, 0x70, 0x00, 0x00]),
    ...section(5, [0x01, 0x00, 0x01]),
    ...section(6, vector(globals)),
    ...section(7, vector(exports)),
    ...section(10, vector(bodies)),
    ...section(11, vector(data)),
  ]);
}

async function waitFor(predicate, message) {
  for (let turn = 0; turn < 80; turn++) {
    if (predicate()) return;
    await new Promise((resolve) => setImmediate(resolve));
  }
  assert.fail(message);
}

const compiled = compiledBlockModule();
const vm = await RV64Debug.create(boundaryModule(compiled));
assert.equal(Object.hasOwn(vm.ex, "chain_next"), false);
assert.equal(Object.hasOwn(vm.ex, "jit_tlb_fill"), false);
assert.equal(Object.hasOwn(vm.ex, "full_system_dispatch_abort"), false);
assert.equal(Number.isFinite(vm.ex.now()), true);
assert.equal(Number.isFinite(vm.ex.unix()), true);

const events = [];
let callbackDepth = 0;
let maximumCallbackDepth = 0;
let nested = false;
vm.onWrite = function (fd, bytes) {
  callbackDepth++;
  maximumCallbackDepth = Math.max(maximumCallbackDepth, callbackDepth);
  try {
    assert.equal(fd, 1);
    assert.deepEqual([...bytes], [1, 2, 3, 4]);
    events.push("write");
    if (nested) {
      nested = false;
      vm.ex.emit_write();
    }
  } finally {
    callbackDepth--;
  }
};
vm.onNetSend = (bytes) => {
  assert.deepEqual([...bytes], [1, 2, 3, 4]);
  events.push("net");
};

vm.ex.emit_both();
assert.deepEqual(events, [], "host callbacks ran inside the Wasm call");
// Mutating linear memory before delivery must not change copied host data.
new Uint8Array(vm.ex.memory.buffer, 0, 4).fill(9);
await waitFor(() => events.length === 2, "host callbacks were not delivered");
assert.deepEqual(events, ["write", "net"]);
assert.equal(maximumCallbackDepth, 1, "host callback delivery recursed");
new Uint8Array(vm.ex.memory.buffer, 0, 4).set([1, 2, 3, 4]);

events.length = 0;
nested = true;
vm.ex.emit_write();
await waitFor(() => events.length === 2, "nested host event was lost");
assert.deepEqual(events, ["write", "write"]);

const callbackFailure = new Error("deferred listener failed");
vm.onWrite = () => { throw callbackFailure; };
vm.ex.emit_write();
await Promise.resolve();
assert.throws(() => vm.ex.ready_status(), (error) => error === callbackFailure);
assert.equal(vm.ex.ready_status(), 7);

// The slot remains live until the deferred retirement boundary.
vm.onWrite = () => {};
const syncModule = new WebAssembly.Module(compiled);
const syncInstance = new WebAssembly.Instance(syncModule, {});
const syncSlot = vm.jitCodeStore.register(syncInstance, ["run"], compiled.length);
assert.equal(syncSlot, 0);
assert.equal(vm.jitCodeStore.snapshot().liveSlots, 1);
vm.ex.retire_slot(syncSlot);
assert.equal(vm.jitCodeStore.snapshot().liveSlots, 1);
await Promise.resolve();
assert.equal(vm.jitCodeStore.snapshot().liveSlots, 0);

async function compileFailureCase(compile) {
  const testVm = await RV64Debug.create(boundaryModule(compiled));
  const originalCompile = WebAssembly.compile;
  const originalWarn = console.warn;
  const warnings = [];
  WebAssembly.compile = compile;
  console.warn = (...args) => warnings.push(args);
  try {
    testVm.ex.trigger_async(11n);
    await waitFor(() => testVm.ex.ready_status() !== 7, "async JIT did not complete");
    assert.equal(testVm.ex.ready_status(), -1);
    assert.equal(testVm.jitCodeStore.snapshot().pendingModules, 0);
    assert.ok(
      warnings.some(([message]) => message === "async jit register failed:"),
      "compile failure was not reported",
    );
  } finally {
    WebAssembly.compile = originalCompile;
    console.warn = originalWarn;
    testVm.destroyJit();
  }
}

await compileFailureCase(() => Promise.reject(new Error("compile rejected")));
const synchronousFailure = new Error("compile threw");
await compileFailureCase(() => { throw synchronousFailure; });

{
  const capacityVm = await RV64Debug.create(boundaryModule(compiled), { maxBytes: 1 });
  capacityVm.ex.trigger_async(12n);
  await waitFor(() => capacityVm.ex.ready_status() === -2, "capacity rejection did not complete");
  assert.equal(capacityVm.jitCodeStore.snapshot().pendingModules, 0);
  capacityVm.destroyJit();
}

{
  const staleVm = await RV64Debug.create(boundaryModule(compiled));
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  const originalCompile = WebAssembly.compile;
  WebAssembly.compile = () => gate;
  try {
    staleVm.ex.trigger_async(13n);
    assert.equal(staleVm.jitCodeStore.snapshot().pendingModules, 1);
    staleVm.jitCodeStore.generation++;
    release(new WebAssembly.Module(compiled));
    await waitFor(() => staleVm.ex.ready_status() === -1, "stale JIT result was installed");
    assert.equal(staleVm.jitCodeStore.snapshot().liveSlots, 0);
  } finally {
    release(new WebAssembly.Module(compiled));
    WebAssembly.compile = originalCompile;
    staleVm.destroyJit();
  }
}

{
  const destroyedVm = await RV64Debug.create(boundaryModule(compiled));
  const originalCompile = WebAssembly.compile;
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  WebAssembly.compile = () => gate;
  try {
    destroyedVm.ex.trigger_async(14n);
    destroyedVm.destroyJit();
    release(new WebAssembly.Module(compiled));
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(destroyedVm.jitCodeStore.snapshot().pendingModules, 0);
    assert.equal(destroyedVm.jitCodeStore.snapshot().liveSlots, 0);
    assert.equal(destroyedVm.ex.ready_status(), 7, "destroyed VM accepted a late JIT result");
  } finally {
    release(new WebAssembly.Module(compiled));
    WebAssembly.compile = originalCompile;
    destroyedVm.destroyJit();
  }
}

vm.destroyJit();
console.log("PASS current host callback and asynchronous JIT boundaries");
