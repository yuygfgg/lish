import assert from "node:assert/strict";
import { RV64Debug } from "../web/rv64.js";

const I32 = 0x7f;
const I64 = 0x7e;

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
const call = (index) => [0x10, ...uleb(index)];

function boundaryModule({ trapReady = false } = {}) {
  const types = [
    functionType([I32, I32, I32], []),
    functionType([I32, I32], []),
    functionType([I64, I32, I32], []),
    functionType([I64], []),
    functionType([I64, I32, I32, I32, I32], []),
    functionType([I64], [I32]),
    functionType([], []),
    functionType([I64, I32], []),
    functionType([], [I32]),
    functionType([I32], []),
    functionType([I32, I32], []),
    functionType([I32, I64, I32], [I64]),
  ];
  const imports = [
    functionImport("host_write", 0),
    functionImport("host_net_send", 1),
    functionImport("host_http_request", 2),
    functionImport("host_wisp_open", 2),
    functionImport("host_wisp_data", 2),
    functionImport("host_wisp_close", 3),
    functionImport("host_wisp_datagram", 4),
    functionImport("host_jit_register_async", 3),
  ];
  const functionTypes = [
    5, 5, 5, 5, 6, 3, 7, 8, 8, 8, 8, 9, 9, 10, 9, 9, 8, 9, 11, 6, 6,
  ];
  const firstFunction = imports.length;
  const functionNames = [
    "user_run",
    "run",
    "sys_run",
    "virt_run",
    "emit",
    "trigger_async",
    "sys_sb_ready",
    "active",
    "ready_status",
    "jit_out_ptr",
    "jit_out_len",
    "virt_console_enable",
    "virt_net_enable",
    "sys_proxy_enable",
    "virt_boot",
    "virt_boot_direct",
    "virt_p9_take_request",
    "chain_next",
    "jit_tlb_fill",
    "trap_dispatch",
    "full_system_dispatch_abort",
  ];
  const exports = functionNames.map((field, index) =>
    functionExport(field, firstFunction + index)
  );
  exports.push([...name("__indirect_function_table"), 0x01, 0x00]);
  exports.push([...name("memory"), 0x02, 0x00]);

  const run = [
    ...i32Const(1), 0x24, 0x00,
    ...i32Const(1), ...i32Const(0), ...i32Const(4), ...call(0),
    ...i32Const(0), ...i32Const(4), ...call(1),
    ...i64Const(3), ...i32Const(0), ...i32Const(4), ...call(2),
    ...i64Const(4), ...i32Const(0), ...i32Const(5), ...call(3),
    ...i64Const(6), ...i32Const(0), ...i32Const(4), ...call(4),
    ...i64Const(7), ...call(5),
    ...i64Const(8), ...i32Const(0), ...i32Const(9),
    ...i32Const(0), ...i32Const(4), ...call(6),
    ...i32Const(0), 0x24, 0x00,
    ...i32Const(0),
  ];
  const emit = [...i32Const(1), ...i32Const(0), ...i32Const(4), ...call(0)];
  const bodies = [
    run,
    run,
    run,
    run,
    emit,
    [0x20, 0x00, ...call(7)],
    trapReady ? [0x00] : [0x20, 0x01, 0x24, 0x01],
    [0x23, 0x00],
    [0x23, 0x01],
    i32Const(64),
    i32Const(4),
    [],
    [],
    [],
    [],
    [],
    i32Const(0),
    [],
    i64Const(-1),
    [...i32Const(1), 0x24, 0x00, 0x00],
    [...i32Const(0), 0x24, 0x00],
  ].map(functionBody);

  const globals = [
    [I32, 0x01, ...i32Const(0), 0x0b],
    [I32, 0x01, ...i32Const(7), 0x0b],
  ];
  const data = [
    [0x00, ...i32Const(0), 0x0b, ...uleb(4), 1, 2, 3, 4],
    [0x00, ...i32Const(64), 0x0b, ...uleb(4), 0, 0, 0, 0],
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

function compiledBlockModule() {
  return new Uint8Array([
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
    ...section(1, vector([functionType([I32], [])])),
    ...section(3, vector([uleb(0)])),
    ...section(7, vector([functionExport("run", 0)])),
    ...section(10, vector([functionBody([])])),
  ]);
}

async function waitFor(predicate, message) {
  for (let turn = 0; turn < 50; turn++) {
    if (predicate()) return;
    await new Promise((resolve) => setImmediate(resolve));
  }
  assert.fail(message);
}

const vm = await RV64Debug.create(boundaryModule(), { maxBytes: 1 });
assert.equal(Object.hasOwn(vm.ex, "chain_next"), false);
assert.equal(Object.hasOwn(vm.ex, "jit_tlb_fill"), false);
assert.equal(Object.hasOwn(vm.ex, "full_system_dispatch_abort"), false);
assert.throws(() => vm.ex.trap_dispatch(), WebAssembly.RuntimeError);
assert.equal(vm.ex.active(), 0, "a trapped dispatch retained active context");
const events = [];
let callbackDepth = 0;
let maximumCallbackDepth = 0;
let emitNestedWrite = false;

function observe(event) {
  return function (...args) {
    callbackDepth++;
    maximumCallbackDepth = Math.max(maximumCallbackDepth, callbackDepth);
    try {
      assert.equal(this, vm);
      assert.equal(vm.ex.active(), 0, `${event} ran before Wasm returned`);
      events.push(event);
      if (event === "write" && emitNestedWrite) {
        emitNestedWrite = false;
        vm.ex.emit();
      }
      if (args.at(-1) instanceof Uint8Array) {
        assert.deepEqual([...args.at(-1)], [1, 2, 3, 4]);
      }
    } finally {
      callbackDepth--;
    }
  };
}

vm.onWrite = observe("write");
vm.onNetSend = observe("net");
vm.onHttpRequest = observe("http");
vm.onWispOpen = observe("wisp-open");
vm.onWispData = observe("wisp-data");
vm.onWispClose = observe("wisp-close");
vm.onWispDatagram = observe("wisp-datagram");

const runEntries = [
  ["runUser", () => vm.runUser(1n)],
  ["run", () => vm.run(1n)],
  ["runSystem", () => vm.runSystem(1n)],
  ["runVirtSystem", () => vm.runVirtSystem(1n)],
];
const expectedEvents = [
  "write",
  "net",
  "http",
  "wisp-open",
  "wisp-data",
  "wisp-close",
  "wisp-datagram",
];
for (const [entry, run] of runEntries) {
  events.length = 0;
  const expectNestedWrite = entry === "runUser";
  emitNestedWrite = expectNestedWrite;
  run();
  assert.deepEqual(
    events,
    expectNestedWrite ? [...expectedEvents, "write"] : expectedEvents,
    `${entry} did not synchronously drain host events`,
  );
}
assert.equal(maximumCallbackDepth, 1, "host-event draining recursed");

events.length = 0;
vm.ex.emit();
assert.deepEqual(events, [], "a non-run export drained synchronously");
await Promise.resolve();
assert.deepEqual(events, ["write"], "the microtask fallback did not drain host events");

const callbackFailure = new Error("deferred listener failed");
vm.onWrite = () => {
  throw callbackFailure;
};
vm.ex.emit();
await Promise.resolve();
assert.throws(
  () => vm.ex.active(),
  (error) => error === callbackFailure,
  "an asynchronous listener failure did not reach the next public boundary",
);
assert.equal(vm.ex.active(), 0, "a delivered listener failure was not cleared");

assert.equal(vm.ex.ready_status(), 7);
vm.ex.trigger_async(33n);
assert.equal(vm.ex.ready_status(), 7, "capacity rejection reentered Wasm");
await Promise.resolve();
assert.equal(vm.ex.ready_status(), -2, "capacity rejection did not complete asynchronously");

const generation = vm.jitCodeStore.generation;
vm.bootVirtLinux({});
assert.equal(vm.jitCodeStore.generation, generation + 1);
vm.bootVirtLinuxDirect({});
assert.equal(vm.jitCodeStore.generation, generation + 2);

{
  const rejectionVm = await RV64Debug.create(boundaryModule());
  const originalWarn = console.warn;
  const warnings = [];
  console.warn = (...args) => warnings.push(args);
  try {
    rejectionVm.ex.trigger_async(34n);
    assert.deepEqual(
      {
        modules: rejectionVm.jitCodeStore.snapshot().pendingModules,
        slots: rejectionVm.jitCodeStore.snapshot().pendingSlots,
        bytes: rejectionVm.jitCodeStore.snapshot().pendingBytes,
      },
      { modules: 1, slots: 1, bytes: 4 },
      "native WebAssembly.compile did not reserve its in-flight resources",
    );
    await waitFor(
      () => rejectionVm.ex.ready_status() === -1,
      "native WebAssembly.compile rejection did not complete the Rust ticket",
    );
    const after = rejectionVm.jitCodeStore.snapshot();
    assert.equal(after.pendingModules, 0);
    assert.equal(after.pendingSlots, 0);
    assert.equal(after.pendingBytes, 0);
    assert.equal(after.liveSlots, 0);
    assert.ok(
      warnings.some(([message]) => message === "async jit register failed:"),
      "native WebAssembly.compile rejection was not reported",
    );
  } finally {
    console.warn = originalWarn;
    rejectionVm.destroyJit();
  }
}

{
  const throwingVm = await RV64Debug.create(boundaryModule());
  const originalCompile = WebAssembly.compile;
  const originalWarn = console.warn;
  const syncFailure = new Error("synchronous compile failure");
  const warnings = [];
  WebAssembly.compile = () => {
    throw syncFailure;
  };
  console.warn = (...args) => warnings.push(args);
  try {
    assert.doesNotThrow(
      () => throwingVm.ex.trigger_async(35n),
      "a synchronous compile error escaped through the Wasm host import",
    );
    assert.equal(throwingVm.ex.ready_status(), 7, "sync failure reentered Wasm");
    assert.equal(
      throwingVm.jitCodeStore.snapshot().pendingModules,
      1,
      "serialized compilation did not retain its reservation",
    );
    await waitFor(
      () => throwingVm.ex.ready_status() === -1,
      "synchronous compile failure did not complete the Rust ticket",
    );
    assert.equal(throwingVm.jitCodeStore.snapshot().pendingModules, 0);
    assert.ok(
      warnings.some(([, error]) => error === syncFailure),
      "synchronous compile failure was not reported",
    );
  } finally {
    WebAssembly.compile = originalCompile;
    console.warn = originalWarn;
    throwingVm.destroyJit();
  }
}

{
  const pendingVm = await RV64Debug.create(boundaryModule(), {
    maxModules: 1,
    maxSlots: 1,
    maxBytes: 4,
    growSlots: 1,
  });
  const originalCompile = WebAssembly.compile;
  const compiled = new WebAssembly.Module(compiledBlockModule());
  let releaseCompile;
  const compileGate = new Promise((resolve) => {
    releaseCompile = resolve;
  });
  let compileCalls = 0;
  WebAssembly.compile = () => {
    compileCalls++;
    return compileGate;
  };
  try {
    pendingVm.ex.trigger_async(36n);
    assert.equal(pendingVm.jitCodeStore.snapshot().pendingModules, 1);
    const bootGeneration = pendingVm.jitCodeStore.generation;
    pendingVm.bootVirtLinux({});
    assert.equal(pendingVm.jitCodeStore.generation, bootGeneration + 1);
    assert.equal(
      pendingVm.jitCodeStore.snapshot().pendingModules,
      1,
      "boot released resources still retained by an in-flight compile",
    );

    pendingVm.ex.trigger_async(37n);
    await waitFor(
      () => pendingVm.ex.ready_status() === -2,
      "an old in-flight compile did not enforce the next boot's capacity",
    );
    assert.equal(compileCalls, 1, "capacity rejection launched another compile");
    assert.equal(pendingVm.jitCodeStore.snapshot().pendingModules, 1);

    releaseCompile(compiled);
    await waitFor(
      () => pendingVm.jitCodeStore.snapshot().pendingModules === 0,
      "stale compile did not release its reservation",
    );
    assert.equal(pendingVm.ex.ready_status(), -1);
    assert.equal(pendingVm.jitCodeStore.snapshot().registeredModules, 0);
    assert.equal(pendingVm.jitCodeStore.snapshot().liveSlots, 0);
  } finally {
    releaseCompile(compiled);
    WebAssembly.compile = originalCompile;
    pendingVm.destroyJit();
  }
}

{
  const failingVm = await RV64Debug.create(boundaryModule({ trapReady: true }));
  const originalCompile = WebAssembly.compile;
  const originalWarn = console.warn;
  const compiled = new WebAssembly.Module(compiledBlockModule());
  const warnings = [];
  WebAssembly.compile = async () => compiled;
  console.warn = (...args) => warnings.push(args);
  try {
    const before = failingVm.jitCodeStore.snapshot();
    failingVm.ex.trigger_async(44n);
    for (let turn = 0; turn < 20; turn++) {
      await new Promise((resolve) => setImmediate(resolve));
      if (failingVm.jitCodeStore.snapshot().retiredSlots > before.retiredSlots) break;
    }
    const after = failingVm.jitCodeStore.snapshot();
    assert.equal(after.registeredModules, before.registeredModules + 1);
    assert.equal(after.retiredSlots, before.retiredSlots + 1);
    assert.equal(after.liveSlots, before.liveSlots, "failed async handoff leaked a slot");
    assert.ok(
      warnings.some(([message]) => message === "async jit completion failed:"),
      "failed async handoff was not reported",
    );
  } finally {
    WebAssembly.compile = originalCompile;
    console.warn = originalWarn;
    failingVm.destroyJit();
  }
}

async function checkTerminalDestroy(disableWeakRef) {
  const destroyedVm = await RV64Debug.create(boundaryModule());
  const originalCompile = WebAssembly.compile;
  const OriginalInstance = WebAssembly.Instance;
  const OriginalWeakRef = globalThis.WeakRef;
  const compiled = new WebAssembly.Module(compiledBlockModule());
  let releaseCompile;
  const compileGate = new Promise((resolve) => {
    releaseCompile = resolve;
  });
  let compileCalls = 0;
  let compileSettled = false;
  let instanceCalls = 0;
  WebAssembly.compile = () => {
    compileCalls++;
    return compileGate.then((module) => {
      compileSettled = true;
      return module;
    });
  };
  WebAssembly.Instance = new Proxy(OriginalInstance, {
    construct(target, args) {
      instanceCalls++;
      return Reflect.construct(target, args);
    },
  });
  if (disableWeakRef) globalThis.WeakRef = undefined;
  try {
    destroyedVm.ex.trigger_async(45n);
    assert.equal(destroyedVm.jitCodeStore.snapshot().pendingModules, 1);
    destroyedVm.destroyJit();
    const afterDestroy = destroyedVm.jitCodeStore.snapshot();
    assert.equal(afterDestroy.destroyed, true);
    assert.deepEqual(
      {
        modules: afterDestroy.pendingModules,
        slots: afterDestroy.pendingSlots,
        bytes: afterDestroy.pendingBytes,
      },
      { modules: 0, slots: 0, bytes: 0 },
      "terminal destroy retained a never-settling compile reservation",
    );
    releaseCompile(compiled);
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(compileCalls, 0, "destroyed VM started a queued compilation");
    assert.equal(compileSettled, false, "cancelled compilation unexpectedly settled");
    assert.equal(instanceCalls, 0, "late compile completion instantiated a module");
    assert.equal(destroyedVm.ex.ready_status(), 7, "late compile completion reached Rust");
    const afterSettle = destroyedVm.jitCodeStore.snapshot();
    assert.deepEqual(
      {
        modules: afterSettle.pendingModules,
        slots: afterSettle.pendingSlots,
        bytes: afterSettle.pendingBytes,
        liveSlots: afterSettle.liveSlots,
      },
      { modules: 0, slots: 0, bytes: 0, liveSlots: 0 },
      "late compile completion restored destroyed JIT state",
    );
  } finally {
    releaseCompile(compiled);
    WebAssembly.compile = originalCompile;
    WebAssembly.Instance = OriginalInstance;
    globalThis.WeakRef = OriginalWeakRef;
    destroyedVm.destroyJit();
  }
}

await checkTerminalDestroy(false);
await checkTerminalDestroy(true);

console.log("PASS deferred host callback and transactional async JIT boundaries");
