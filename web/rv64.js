import {
  isNativeDisk,
  NativeDiskClient,
  serviceNativeDiskRequest,
} from "./native-disk.js";

// Lish browser runtime for the rv64-wasm module.
//
// The runtime talks to plain extern "C" exports over Wasm linear memory.
// No bundler, no wasm-bindgen glue; works as an ES module in browsers and Node.

const DEFAULT_JIT_LIMITS = Object.freeze({
  maxModules: 32_768,
  maxSlots: 65_536,
  maxBytes: 128 * 1024 * 1024,
  growSlots: 4096,
});

const MAX_ASYNC_JIT_COMPILERS = 4;
const MAX_FULL_SYSTEM_PENDING_JIT = 4;

function asyncJitCompilerCount(value) {
  if (value === undefined) return MAX_ASYNC_JIT_COMPILERS;
  if (!Number.isSafeInteger(value) || value < 1 || value > MAX_FULL_SYSTEM_PENDING_JIT) {
    throw new RangeError(
      `jit.asyncCompilers must be an integer from 1 to ${MAX_FULL_SYSTEM_PENDING_JIT}`,
    );
  }
  return value;
}

// These exports can emit a large stream of host events. Deliver their queued
// events before the public run method returns. All other Wasm entries use the
// microtask fallback so a host import never calls application code reentrantly.
const SYNCHRONOUS_HOST_DRAIN_EXPORTS = new Set([
  "virt_run",
]);

// Generated JIT modules call these with an ephemeral execution context. They
// are internal callbacks, not safe low-level entry points for embedders.
const INTERNAL_WASM_CALLBACK_EXPORTS = new Set([
  "chain_next",
  "full_system_dispatch_abort",
  "jit_tlb_fill",
]);

class OwnerReference {
  constructor(value) {
    if (typeof globalThis.WeakRef === "function") {
      this.weakTarget = new globalThis.WeakRef(value);
      this.weakStore = new globalThis.WeakRef(value.jitCodeStore);
      this.strongTarget = undefined;
    } else {
      this.weakTarget = undefined;
      this.weakStore = undefined;
      this.strongTarget = value;
    }
  }

  deref() {
    return this.weakTarget?.deref() ?? this.strongTarget;
  }

  derefStore() {
    return this.weakStore?.deref() ?? this.strongTarget?.jitCodeStore;
  }

  clear() {
    this.weakTarget = undefined;
    this.weakStore = undefined;
    this.strongTarget = undefined;
  }
}

/**
 * Owns all dynamically compiled WebAssembly functions for one VM.
 *
 * Rust retires table indexes while a VM slice is active. This store clears
 * and reuses those indexes only after control returns to JavaScript. A module
 * remains live until its final exported function leaves the table.
 */
export class JitCodeStore {
  constructor(table, limits = {}) {
    if (!(table instanceof WebAssembly.Table)) {
      throw new TypeError("JitCodeStore requires a WebAssembly.Table");
    }
    this.table = table;
    this.base = table.length;
    this.next = this.base;
    this.limits = Object.freeze({ ...DEFAULT_JIT_LIMITS, ...limits });
    for (const name of ["maxModules", "maxSlots", "maxBytes", "growSlots"]) {
      const value = this.limits[name];
      if (!Number.isSafeInteger(value) || value <= 0) {
        throw new RangeError(`jit.${name} must be a positive safe integer`);
      }
    }
    this.free = [];
    this.owners = new Map();
    this.slotOwners = new Map();
    this.retirements = new Map();
    this.reservations = new Map();
    this.nextOwner = 1;
    this.nextReservation = 1;
    this.generation = 0;
    this.liveSlots = 0;
    this.liveBytes = 0;
    this.pendingSlots = 0;
    this.pendingBytes = 0;
    this.peakModules = 0;
    this.peakSlots = 0;
    this.peakBytes = 0;
    this.peakPendingModules = 0;
    this.peakPendingSlots = 0;
    this.peakPendingBytes = 0;
    this.peakCommittedModules = 0;
    this.peakCommittedSlots = 0;
    this.peakCommittedBytes = 0;
    this.emittedBytes = 0;
    this.registeredModules = 0;
    this.retiredModules = 0;
    this.retiredSlots = 0;
    this.evictedModules = 0;
    this.evictedSlots = 0;
    this.capacityRejects = 0;
    this.registrationFailures = 0;
    this.destroyed = false;
  }

  canRegister(slotCount, byteLength) {
    return !this.destroyed &&
      Number.isSafeInteger(slotCount) &&
      slotCount > 0 &&
      Number.isSafeInteger(byteLength) &&
      byteLength >= 0 &&
      this.owners.size + this.reservations.size < this.limits.maxModules &&
      this.liveSlots + this.pendingSlots + slotCount <= this.limits.maxSlots &&
      this.liveBytes + this.pendingBytes + byteLength <= this.limits.maxBytes &&
      this.#hasRun(slotCount);
  }

  register(instance, exportNames, byteLength) {
    if (!this.canRegister(exportNames.length, byteLength)) {
      this.capacityRejects++;
      return -2;
    }
    const functions = this.#getFunctions(instance, exportNames);
    const base = this.#allocate(functions.length);
    return this.#install(base, functions, byteLength);
  }

  /** Reserve bounded capacity and table slots for one asynchronous module. */
  reserve(slotCount, byteLength) {
    if (!this.canRegister(slotCount, byteLength)) {
      this.capacityRejects++;
      return null;
    }
    const base = this.#allocate(slotCount);
    try {
      const reservation = Object.freeze({
        id: this.nextReservation++,
        generation: this.generation,
        base,
        slotCount,
        byteLength,
      });
      this.reservations.set(reservation, reservation);
      this.pendingSlots += slotCount;
      this.pendingBytes += byteLength;
      this.#recordPendingPeaks();
      return reservation;
    } catch (error) {
      this.#releaseRun(base, slotCount);
      throw error;
    }
  }

  /** Release an asynchronous module reservation that did not become live. */
  releaseReservation(reservation) {
    const active = this.reservations.get(reservation);
    if (active === undefined) return false;
    this.reservations.delete(reservation);
    this.pendingSlots -= active.slotCount;
    this.pendingBytes -= active.byteLength;
    this.#releaseRun(active.base, active.slotCount);
    return true;
  }

  /** Atomically turn an asynchronous reservation into a live module owner. */
  registerReserved(reservation, instance, exportNames) {
    const active = this.reservations.get(reservation);
    if (active === undefined) {
      throw new TypeError("JIT reservation is not active");
    }
    if (active.generation !== this.generation) {
      this.releaseReservation(reservation);
      return -1;
    }
    try {
      if (exportNames.length !== active.slotCount) {
        this.registrationFailures++;
        throw new RangeError("JIT reservation slot count does not match its exports");
      }
      const functions = this.#getFunctions(instance, exportNames);
      this.#consumeReservation(active);
      return this.#install(active.base, functions, active.byteLength);
    } catch (error) {
      this.releaseReservation(reservation);
      throw error;
    }
  }

  #getFunctions(instance, exportNames) {
    const functions = exportNames.map((name) => instance.exports[name]);
    if (functions.some((fn) => typeof fn !== "function")) {
      this.registrationFailures++;
      throw new TypeError("JIT module has a missing function export");
    }
    return functions;
  }

  #install(base, functions, byteLength) {
    const ownerId = this.nextOwner++;
    const owner = {
      bytes: byteLength,
      evicted: false,
      slots: new Set(),
    };
    try {
      for (let i = 0; i < functions.length; i++) {
        const slot = base + i;
        this.table.set(slot, functions[i]);
        owner.slots.add(slot);
        this.slotOwners.set(slot, ownerId);
      }
    } catch (error) {
      for (const slot of owner.slots) {
        this.table.set(slot, null);
        this.slotOwners.delete(slot);
      }
      this.#releaseRun(base, functions.length);
      this.registrationFailures++;
      throw error;
    }
    this.owners.set(ownerId, owner);
    this.liveSlots += functions.length;
    this.liveBytes += byteLength;
    this.emittedBytes += byteLength;
    this.registeredModules++;
    this.#recordLivePeaks();
    return base;
  }

  /** Queue one slot for cleanup. reason=1 identifies policy eviction. */
  retire(slot, reason = 0) {
    if (!this.slotOwners.has(slot)) return;
    this.retirements.set(slot, Math.max(this.retirements.get(slot) ?? 0, reason));
  }

  /** Clear queued slots at a JavaScript boundary, never in a Wasm import. */
  flushRetired() {
    if (this.retirements.size === 0) return;
    const slots = [...this.retirements].sort((a, b) => a[0] - b[0]);
    this.retirements.clear();
    const released = [];
    for (const [slot, reason] of slots) {
      const ownerId = this.slotOwners.get(slot);
      if (ownerId === undefined) continue;
      const owner = this.owners.get(ownerId);
      this.table.set(slot, null);
      this.slotOwners.delete(slot);
      owner.slots.delete(slot);
      owner.evicted ||= reason === 1;
      this.liveSlots--;
      this.retiredSlots++;
      if (reason === 1) this.evictedSlots++;
      released.push(slot);
      if (owner.slots.size === 0) {
        this.owners.delete(ownerId);
        this.liveBytes -= owner.bytes;
        this.retiredModules++;
        if (owner.evicted) this.evictedModules++;
      }
    }
    for (let i = 0; i < released.length;) {
      let end = i + 1;
      while (end < released.length && released[end] === released[end - 1] + 1) end++;
      this.#releaseRun(released[i], end - i);
      i = end;
    }
  }

  clear() {
    if (this.destroyed) return;
    this.generation++;
    for (const slot of this.slotOwners.keys()) this.retirements.set(slot, 0);
    this.flushRetired();
  }

  destroy() {
    if (this.destroyed) return;
    this.destroyed = true;
    this.generation++;
    for (const slot of this.slotOwners.keys()) this.retirements.set(slot, 0);
    this.flushRetired();
    for (const reservation of [...this.reservations.keys()]) {
      this.releaseReservation(reservation);
    }
  }

  snapshot() {
    return Object.freeze({
      liveModules: this.owners.size,
      liveSlots: this.liveSlots,
      pendingModules: this.reservations.size,
      pendingSlots: this.pendingSlots,
      pendingBytes: this.pendingBytes,
      freeSlots: this.free.reduce((sum, run) => sum + run.length, 0),
      tableHighWater: this.next - this.base,
      tableCapacity: this.table.length - this.base,
      liveBytes: this.liveBytes,
      emittedBytes: this.emittedBytes,
      peakModules: this.peakModules,
      peakSlots: this.peakSlots,
      peakBytes: this.peakBytes,
      peakPendingModules: this.peakPendingModules,
      peakPendingSlots: this.peakPendingSlots,
      peakPendingBytes: this.peakPendingBytes,
      peakCommittedModules: this.peakCommittedModules,
      peakCommittedSlots: this.peakCommittedSlots,
      peakCommittedBytes: this.peakCommittedBytes,
      registeredModules: this.registeredModules,
      retiredModules: this.retiredModules,
      retiredSlots: this.retiredSlots,
      evictedModules: this.evictedModules,
      evictedSlots: this.evictedSlots,
      capacityRejects: this.capacityRejects,
      registrationFailures: this.registrationFailures,
      destroyed: this.destroyed,
      limits: this.limits,
    });
  }

  #consumeReservation(reservation) {
    this.reservations.delete(reservation);
    this.pendingSlots -= reservation.slotCount;
    this.pendingBytes -= reservation.byteLength;
  }

  #recordLivePeaks() {
    this.peakModules = Math.max(this.peakModules, this.owners.size);
    this.peakSlots = Math.max(this.peakSlots, this.liveSlots);
    this.peakBytes = Math.max(this.peakBytes, this.liveBytes);
    this.#recordCommittedPeaks();
  }

  #recordPendingPeaks() {
    this.peakPendingModules = Math.max(
      this.peakPendingModules,
      this.reservations.size,
    );
    this.peakPendingSlots = Math.max(this.peakPendingSlots, this.pendingSlots);
    this.peakPendingBytes = Math.max(this.peakPendingBytes, this.pendingBytes);
    this.#recordCommittedPeaks();
  }

  #recordCommittedPeaks() {
    this.peakCommittedModules = Math.max(
      this.peakCommittedModules,
      this.owners.size + this.reservations.size,
    );
    this.peakCommittedSlots = Math.max(
      this.peakCommittedSlots,
      this.liveSlots + this.pendingSlots,
    );
    this.peakCommittedBytes = Math.max(
      this.peakCommittedBytes,
      this.liveBytes + this.pendingBytes,
    );
  }

  #hasRun(length) {
    return this.free.some((run) => run.length >= length) ||
      this.next + length <= this.base + this.limits.maxSlots;
  }

  #allocate(length) {
    const freeIndex = this.free.findIndex((run) => run.length >= length);
    if (freeIndex >= 0) {
      const run = this.free[freeIndex];
      const base = run.start;
      run.start += length;
      run.length -= length;
      if (run.length === 0) this.free.splice(freeIndex, 1);
      return base;
    }
    const base = this.next;
    const required = base + length;
    if (required > this.table.length) {
      const maximum = this.base + this.limits.maxSlots;
      const growth = Math.min(
        Math.max(this.limits.growSlots, required - this.table.length),
        maximum - this.table.length,
      );
      if (growth <= 0) throw new RangeError("JIT table capacity exhausted");
      this.table.grow(growth);
    }
    this.next = required;
    return base;
  }

  #releaseRun(start, length) {
    if (length === 0) return;
    let index = this.free.findIndex((run) => run.start > start);
    if (index < 0) index = this.free.length;
    this.free.splice(index, 0, { start, length });
    if (index > 0) {
      const previous = this.free[index - 1];
      const current = this.free[index];
      if (previous.start + previous.length === current.start) {
        previous.length += current.length;
        this.free.splice(index, 1);
        index--;
      }
    }
    const current = this.free[index];
    const next = this.free[index + 1];
    if (next && current.start + current.length === next.start) {
      current.length += next.length;
      this.free.splice(index + 1, 1);
    }
  }
}

// Low-level bindings used by Lish and the repository's architecture
// and differential tests. This is intentionally not the supported embedding
// API; applications should use RV64 below.
export class RV64Debug {
  #wasmExports;
  #generatedModuleImports;
  #wasmCallDepth = 0;
  #hostCalls = [];
  #hostDrainScheduled = false;
  #drainingHostCalls = false;
  #deferredHostFailure;
  #hasDeferredHostFailure = false;
  #asyncOwners = new Set();
  #jitCompileQueue = [];
  #jitCompileActive = 0;
  #jitCompileQueued = 0;
  #peakJitCompileQueued = 0;
  #jitCompileCount = 0;
  #jitCompileMs = 0;
  #maxJitCompileMs = 0;
  #maxAsyncJitCompilers = MAX_ASYNC_JIT_COMPILERS;

  /** @param {WebAssembly.Instance} instance */
  constructor(instance) {
    this.#wasmExports = instance.exports;
    const exports = Object.create(null);
    for (const [name, value] of Object.entries(this.#wasmExports)) {
      if (INTERNAL_WASM_CALLBACK_EXPORTS.has(name)) continue;
      const exposed = typeof value === "function"
        ? (...args) => {
            this.#throwDeferredHostFailure();
            return this.#callWasm(
              value,
              args,
              SYNCHRONOUS_HOST_DRAIN_EXPORTS.has(name),
            );
          }
        : value;
      Object.defineProperty(exports, name, { enumerable: true, value: exposed });
    }
    this.ex = Object.freeze(exports);
    /** Override to capture guest console output: (fd, Uint8Array) => void */
    this.onWrite = (fd, bytes) => {
      const text = new TextDecoder().decode(bytes);
      (fd === 2 ? console.error : console.log)(text);
    };
    /** Called with each Ethernet frame the guest sends; set by connectNet. */
    this.onNetSend = () => {};
    this.jitRetirementFlushScheduled = false;
  }

  #jitModuleImports() {
    return (this.#generatedModuleImports ??= {
      env: {
        memory: this.#wasmExports.memory,
        tlb_fill: this.#wasmExports.jit_tlb_fill,
        chain_next: this.#wasmExports.chain_next,
        __indirect_function_table: this.#wasmExports.__indirect_function_table,
      },
    });
  }

  #retainAsyncOwner() {
    const owner = new OwnerReference(this);
    this.#asyncOwners.add(owner);
    return owner;
  }

  static #instantiateAsyncJit(owner, module) {
    const target = owner.deref();
    if (!target || target.jitCodeStore.destroyed) return -1;
    return new WebAssembly.Instance(module, target.#jitModuleImports());
  }

  #enqueueAsyncJit(bytes, owner) {
    this.#jitCompileQueued++;
    this.#peakJitCompileQueued = Math.max(
      this.#peakJitCompileQueued,
      this.#jitCompileQueued,
    );
    const completion = new Promise((resolve, reject) => {
      this.#jitCompileQueue.push({ bytes, owner, reject, resolve });
    });
    this.#pumpAsyncJit();
    return completion;
  }

  #pumpAsyncJit() {
    while (
      this.#jitCompileActive < this.#maxAsyncJitCompilers &&
      this.#jitCompileQueue.length !== 0
    ) {
      const job = this.#jitCompileQueue.shift();
      this.#jitCompileQueued--;
      if (this.jitCodeStore.destroyed || !job.owner.deref()) {
        job.resolve(-1);
        continue;
      }
      this.#jitCompileActive++;
      const started = performance.now();
      Promise.resolve()
        .then(() => {
          if (this.jitCodeStore.destroyed || !job.owner.deref()) return -1;
          return WebAssembly.compile(job.bytes);
        })
        .then((module) => typeof module === "number"
          ? module
          : RV64Debug.#instantiateAsyncJit(job.owner, module))
        .then(job.resolve, job.reject)
        .finally(() => {
          const elapsed = performance.now() - started;
          this.#jitCompileCount++;
          this.#jitCompileMs += elapsed;
          this.#maxJitCompileMs = Math.max(this.#maxJitCompileMs, elapsed);
          this.#jitCompileActive--;
          this.#pumpAsyncJit();
        });
    }
  }

  #callWasm(fn, args, synchronousDrain) {
    if (this.#wasmCallDepth !== 0) {
      throw new Error("reentrant Wasm calls are not allowed");
    }

    this.#wasmCallDepth = 1;
    let result;
    let failure;
    let failed = false;
    let wasmFailed = false;
    const recordFailure = (error) => {
      if (!failed) {
        failure = error;
        failed = true;
      }
    };
    try {
      result = fn(...args);
    } catch (error) {
      wasmFailed = true;
      recordFailure(error);
    }
    if (wasmFailed) {
      try {
        this.#wasmExports.full_system_dispatch_abort?.();
      } catch (error) {
        recordFailure(error);
      }
    }
    this.#wasmCallDepth = 0;

    if (synchronousDrain) {
      try {
        if (this.jitCodeStore) this.flushJitRetirements();
      } catch (error) {
        recordFailure(error);
      }
      try {
        this.#drainHostCalls();
      } catch (error) {
        recordFailure(error);
      }
    } else {
      this.#scheduleHostDrain();
    }
    if (failed) throw failure;
    return result;
  }

  #deferHostCall(fn, receiver, args) {
    this.#hostCalls.push({ fn, receiver, args });
    if (this.#wasmCallDepth === 0) this.#scheduleHostDrain();
  }

  #throwDeferredHostFailure() {
    if (!this.#hasDeferredHostFailure) return;
    const error = this.#deferredHostFailure;
    this.#deferredHostFailure = undefined;
    this.#hasDeferredHostFailure = false;
    throw error;
  }

  #scheduleHostDrain() {
    if (
      this.#hostCalls.length === 0 ||
      this.#hostDrainScheduled ||
      this.#drainingHostCalls
    ) {
      return;
    }
    this.#hostDrainScheduled = true;
    queueMicrotask(() => {
      this.#hostDrainScheduled = false;
      if (this.#wasmCallDepth !== 0) {
        this.#scheduleHostDrain();
        return;
      }
      try {
        this.#drainHostCalls();
      } catch (error) {
        if (!this.#hasDeferredHostFailure) {
          this.#deferredHostFailure = error;
          this.#hasDeferredHostFailure = true;
        }
      }
    });
  }

  #drainHostCalls() {
    if (this.#wasmCallDepth !== 0 || this.#drainingHostCalls) return;
    this.#drainingHostCalls = true;
    let firstFailure;
    let failed = false;
    try {
      while (this.#hostCalls.length !== 0) {
        const calls = this.#hostCalls.splice(0);
        for (const { fn, receiver, args } of calls) {
          try {
            Reflect.apply(fn, receiver, args);
          } catch (error) {
            if (!failed) {
              firstFailure = error;
              failed = true;
            }
          }
        }
      }
    } finally {
      this.#drainingHostCalls = false;
    }
    if (failed) throw firstFailure;
  }

  #reportAsyncJitError(message, error) {
    try {
      console.warn(message, error);
    } catch {
      // Diagnostics must not turn a contained background failure into an
      // unhandled Promise rejection.
    }
  }

  static #reportAsyncJitTaskFailure(error) {
    try {
      console.warn("async jit task failed:", error);
    } catch {
      // A fire-and-forget task must never create another rejected Promise.
    }
  }

  static async #completeAsyncJit(ticket, slotCount, completion, reservation, owner) {
    try {
      let result;
      try {
        result = await completion;
      } catch (error) {
        const target = owner.deref();
        if (target) target.#reportAsyncJitError("async jit register failed:", error);
        result = -1;
      }

      const target = owner.deref();
      if (!target || target.jitCodeStore.destroyed) return;

      let idx = typeof result === "number" ? result : -1;
      if (typeof result !== "number") {
        try {
          const names = slotCount === 1
            ? ["run"]
            : Array.from({ length: slotCount }, (_, index) => `r${index}`);
          idx = target.jitCodeStore.registerReserved(
            reservation,
            result,
            names,
          );
          if (idx >= 0) {
            target.jitBlocks = (target.jitBlocks ?? 0) + slotCount;
            if (slotCount > 1) target.jitBatches = (target.jitBatches ?? 0) + 1;
          }
        } catch (error) {
          target.#reportAsyncJitError("async jit register failed:", error);
          idx = -1;
        }
      }

      let orphan = idx >= 0 ? idx : -1;
      try {
        // Installation and Rust ownership handoff share this Promise job, so
        // clear/reset cannot reuse the slot between the two operations.
        const ready = target.#wasmExports.sys_jit_ready ?? target.#wasmExports.sys_sb_ready;
        const args = target.#wasmExports.sys_jit_ready
          ? [ticket, idx, slotCount]
          : [ticket, idx];
        target.#callWasm(ready, args, false);
        orphan = -1;
      } catch (error) {
        if (orphan >= 0) target.jitCodeStore.retire(orphan);
        target.#reportAsyncJitError("async jit completion failed:", error);
      } finally {
        try {
          target.flushJitRetirements();
        } catch (error) {
          target.#reportAsyncJitError("async jit retirement failed:", error);
        }
      }
    } finally {
      const target = owner.deref();
      try {
        if (reservation !== null) {
          owner.derefStore()?.releaseReservation(reservation);
        }
      } catch (error) {
        if (target) target.#reportAsyncJitError("async jit cleanup failed:", error);
      }
      if (target) target.#asyncOwners.delete(owner);
      owner.clear();
    }
  }

  /** Instantiate from wasm bytes (ArrayBuffer/TypedArray/Response). */
  static async create(wasmSource, jitOptions = {}) {
    const { asyncCompilers, enabled = true, ...jitStoreOptions } = jitOptions;
    if (typeof enabled !== "boolean") {
      throw new TypeError("jit.enabled must be a boolean");
    }
    let vm;
    const imports = {
      env: {
        host_write: (fd, ptr, len) => {
          // Copy out: the view dies if wasm memory grows.
          const bytes = new Uint8Array(vm.#wasmExports.memory.buffer, ptr, len).slice();
          vm.#deferHostCall(vm.onWrite, vm, [fd, bytes]);
        },
        // One Ethernet frame the guest transmitted. Goes straight out the
        // relay socket — one binary message per frame, websockproxy's protocol.
        host_net_send: (ptr, len) => {
          const frame = new Uint8Array(vm.#wasmExports.memory.buffer, ptr, len).slice();
          vm.#deferHostCall(vm.onNetSend, vm, [frame]);
        },
        host_now_ms: () =>
          typeof performance !== "undefined" ? performance.now() : Date.now(),
        host_unix_ms: () => Date.now(),
        host_jit_retire: (idx, reason) => {
          vm.jitCodeStore.retire(idx, reason);
          vm.scheduleJitRetirementFlush();
        },
        // All full-system modules use this path. WebAssembly.compile can run
        // outside the guest slice; the reservation keeps every batch's table
        // slots contiguous until its exports are installed atomically.
        host_jit_register_async: (ticket, slotCount = 1) => {
          let reservation = null;
          let completion;
          const owner = vm.#retainAsyncOwner();
          try {
            const bytes = new Uint8Array(
              vm.#wasmExports.memory.buffer,
              vm.#wasmExports.jit_out_ptr(),
              vm.#wasmExports.jit_out_len(),
            ).slice();
            reservation = vm.jitCodeStore.reserve(slotCount, bytes.length);
            if (reservation === null) {
              completion = Promise.resolve(-2);
            } else {
              completion = vm.#enqueueAsyncJit(bytes, owner);
            }
          } catch (error) {
            if (reservation !== null) {
              vm.jitCodeStore.releaseReservation(reservation);
            }
            vm.#reportAsyncJitError("async jit register failed:", error);
            completion = Promise.resolve(-1);
          }
          const task = RV64Debug.#completeAsyncJit(
            ticket,
            slotCount,
            completion,
            reservation,
            owner,
          );
          void task.catch(RV64Debug.#reportAsyncJitTaskFailure);
        },
      },
    };
    const { instance } =
      wasmSource instanceof Response || wasmSource instanceof Promise
        ? await WebAssembly.instantiateStreaming(wasmSource, imports)
        : await WebAssembly.instantiate(wasmSource, imports);
    vm = new RV64Debug(instance);
    vm.jitCodeStore = new JitCodeStore(
      vm.#wasmExports.__indirect_function_table,
      jitStoreOptions,
    );
    if (!enabled) vm.ex.jit_set_enabled?.(0);
    vm.#maxAsyncJitCompilers = asyncJitCompilerCount(asyncCompilers);
    // Hardware FMA: use f64x2.relaxed_madd for the guest's FMADD family iff
    // the engine validates it AND it is fused on this hardware (the spec
    // allows unfused; only fused is bit-exact). Probe empirically:
    // a=b=1+2^-52, c=-(1+2^-51) gives 2^-104 fused, 0 unfused.
    try {
      const sec = (id, p) => [id, p.length, ...p];
      const code = [0x00,
        0x20, 0, 0xfd, 0x12, 0x20, 1, 0xfd, 0x12, 0x20, 2, 0xfd, 0x12,
        0xfd, 0x87, 0x02, 0xfd, 0x21, 0x00, 0x0b];
      const mod = new WebAssembly.Module(new Uint8Array([
        0, 0x61, 0x73, 0x6d, 1, 0, 0, 0,
        ...sec(1, [1, 0x60, 3, 0x7e, 0x7e, 0x7e, 1, 0x7c]),
        ...sec(3, [1, 0]),
        ...sec(7, [1, 1, 0x74, 0, 0]),
        ...sec(10, [1, code.length, ...code]),
      ]));
      const bits = (x) => new BigInt64Array(new Float64Array([x]).buffer)[0];
      const r = new WebAssembly.Instance(mod, {}).exports.t(
        bits(1 + 2 ** -52), bits(1 + 2 ** -52), bits(-(1 + 2 ** -51)));
      if (r !== 0 && Math.abs(r - 2 ** -104) < 2 ** -150) {
        vm.ex.jit_set_hw_fma?.(1);
      }
    } catch {
      /* no relaxed SIMD: the exact emulated fma stays in charge */
    }
    // Direct block chaining needs wasm tail calls (return_call_indirect,
    // shipped by default in V8 11.2+). Feature-detect with a 1-function probe
    // so older engines just keep the plain dispatch loop.
    try {
      new WebAssembly.Module(new Uint8Array([
        0, 0x61, 0x73, 0x6d, 1, 0, 0, 0,
        1, 5, 1, 0x60, 1, 0x7f, 0,
        2, 11, 1, 1, 0x65, 3, 0x74, 0x61, 0x62, 0x01, 0x70, 0, 0,
        3, 2, 1, 0,
        10, 11, 1, 9, 0, 0x20, 0, 0x41, 0, 0x13, 0, 0, 0x0b,
      ]));
      // Chaining is DEFAULT OFF after three measured architectures:
      // (1) emitted return_call_indirect — ~2ns/hop on node 20.18.1, but
      // any module importing the shared table makes table.set O(importing
      // instances): quadratic registration for tcc/CPython populations;
      // (2) per-module shared helper — same import, same quadratic;
      // (3) env.chain_next, a host-module Rust dispatch reached as a
      // function import (no table import, no quadratic) — measured SLOWER
      // everywhere (nbench ASSIGNMENT 8.3 -> 6.2, python 4.6 -> 6.2s):
      // the host dispatch loop is already wasm with no JS frame, so the
      // sandwich (block -> host Rust -> block) re-does the loop's own
      // bookkeeping plus two extra call frames per hop. The per-dispatch
      // cost is the bookkeeping itself, not a boundary. RV_TAILCALL=1
      // re-enables for experiments.
      if (globalThis.process?.env?.RV_TAILCALL === "1") {
        vm.ex.jit_set_tailcall?.(1);
      }
    } catch {
      /* no tail calls: chaining stays off */
    }
    return vm;
  }

  flushJitRetirements() {
    this.jitRetirementFlushScheduled = false;
    this.jitCodeStore.flushRetired();
  }

  scheduleJitRetirementFlush() {
    if (this.jitRetirementFlushScheduled) return;
    this.jitRetirementFlushScheduled = true;
    queueMicrotask(() => this.flushJitRetirements());
  }

  jitMetrics() {
    return Object.freeze({
      ...this.jitCodeStore.snapshot(),
      rustLiveSlots: Number(this.ex.jit_stat(73)),
      rustPeakSlots: Number(this.ex.jit_stat(74)),
      rustRetiredSlots: Number(this.ex.jit_stat(75)),
      rustEvictedOwners: Number(this.ex.jit_stat(76)),
      rustEvictedSlots: Number(this.ex.jit_stat(77)),
      rustCapacityRejects: Number(this.ex.jit_stat(78)),
      rustPendingBuilds: Number(this.ex.sys_pending_builds?.() ?? 0),
      pendingBlocks: Number(this.ex.jit_stat(79)),
      pendingBatches: Number(this.ex.jit_stat(80)),
      pendingRegions: Number(this.ex.jit_stat(81)),
      asyncCompileActive: this.#jitCompileActive,
      asyncCompileQueued: this.#jitCompileQueued,
      peakAsyncCompileQueued: this.#peakJitCompileQueued,
      asyncCompileCount: this.#jitCompileCount,
      asyncCompileMs: this.#jitCompileMs,
      maxAsyncCompileMs: this.#maxJitCompileMs,
    });
  }

  pendingJitBuilds() {
    return Number(this.ex.sys_pending_builds?.() ?? 0);
  }

  destroyJit() {
    for (const owner of this.#asyncOwners) owner.clear();
    this.#asyncOwners.clear();
    this.jitCodeStore.destroy();
    this.#pumpAsyncJit();
  }

}

// ---- full-system API ------------------------------------------------------

/** Run a slice of the booted system. Returns true when powered off. */
/** Boot the modern OpenSBI/Linux virt machine. */
RV64Debug.prototype.bootVirtLinux = function ({
  opensbi,
  kernel,
  initrd,
  disk,
  externalDiskSize,
  cmdline,
  ramMB = 512,
  net = false,
  netMac,
}) {
  const stage = (bytes, fn) => {
    if (!bytes) return;
    const ptr = this.ex.staging_alloc(bytes.length);
    new Uint8Array(this.ex.memory.buffer, ptr, bytes.length).set(bytes);
    fn();
  };
  stage(opensbi, () => this.ex.virt_stage_opensbi());
  stage(kernel, () => this.ex.virt_stage_kernel());
  stage(initrd, () => this.ex.virt_stage_initrd());
  stage(disk, () => this.ex.virt_stage_disk());
  if (externalDiskSize !== undefined) this.ex.virt_external_disk_size(BigInt(externalDiskSize));
  if (cmdline) stage(new TextEncoder().encode(cmdline), () => this.ex.virt_stage_cmdline());
  if (netMac) stage(new Uint8Array(netMac), () => this.ex.virt_stage_net_mac());
  this.ex.virt_net_enable(net ? 1 : 0);
  this.jitCodeStore.generation++;
  this.ex.virt_boot(ramMB);
  this.flushJitRetirements();
};

/** Assemble riscv-virt and enter Linux directly in S-mode. */
RV64Debug.prototype.bootVirtLinuxDirect = function ({
  kernel,
  initrd,
  disk,
  externalDiskSize,
  cmdline,
  ramMB = 512,
  net = false,
  netMac,
}) {
  const stage = (bytes, fn) => {
    if (!bytes) return;
    const ptr = this.ex.staging_alloc(bytes.length);
    new Uint8Array(this.ex.memory.buffer, ptr, bytes.length).set(bytes);
    fn();
  };
  stage(kernel, () => this.ex.virt_stage_kernel());
  stage(initrd, () => this.ex.virt_stage_initrd());
  stage(disk, () => this.ex.virt_stage_disk());
  if (externalDiskSize !== undefined) this.ex.virt_external_disk_size(BigInt(externalDiskSize));
  if (cmdline) stage(new TextEncoder().encode(cmdline), () => this.ex.virt_stage_cmdline());
  if (netMac) stage(new Uint8Array(netMac), () => this.ex.virt_stage_net_mac());
  this.ex.virt_net_enable(net ? 1 : 0);
  this.jitCodeStore.generation++;
  this.ex.virt_boot_direct(ramMB);
  this.flushJitRetirements();
};

/** Run a slice of the modern virt machine. Returns true when powered off. */
RV64Debug.prototype.runVirtSystem = function (maxInsns = 2_000_000n) {
  return this.ex.virt_run(BigInt(maxInsns)) === 1;
};

RV64Debug.prototype.virtRunSystemOutcome = function (maxInsns = 2_000_000n) {
  return this.ex.virt_run(BigInt(maxInsns));
};

RV64Debug.prototype.virtDiskRequest = function () {
  const kind = Number(this.ex.virt_disk_request_kind?.() ?? 0);
  if (!kind) return null;
  const request = {
    id: this.ex.virt_disk_request_id(),
    kind: ["", "read", "write", "flush"][kind] ?? "unknown",
    offset: this.ex.virt_disk_request_offset(),
    length: this.ex.virt_disk_request_length(),
  };
  if (kind === 2) {
    const ptr = this.ex.virt_disk_request_body();
    request.body = new Uint8Array(this.ex.memory.buffer, ptr, Number(request.length)).slice();
  }
  return request;
};

RV64Debug.prototype.virtDiskComplete = function (bytes, ok = true) {
  if (bytes !== undefined) {
    const body = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    const ptr = this.ex.staging_alloc(body.length);
    new Uint8Array(this.ex.memory.buffer, ptr, body.length).set(body);
  } else {
    this.ex.staging_alloc(0);
  }
  return this.ex.virt_disk_complete(ok ? 1 : 0) !== 0;
};

/** Send keyboard input to the modern machine's 8250 UART. */
RV64Debug.prototype.virtConsoleInput = function (bytes) {
  const ptr = this.ex.staging_alloc(bytes.length);
  new Uint8Array(this.ex.memory.buffer, ptr, bytes.length).set(bytes);
  this.ex.virt_console_input();
};

RV64Debug.prototype.virtNetInput = function (frame) {
  const ptr = this.ex.staging_alloc(frame.length);
  new Uint8Array(this.ex.memory.buffer, ptr, frame.length).set(frame);
  this.ex.virt_net_input();
};

RV64Debug.prototype.virtInsnCount = function () {
  return this.ex.virt_insn_count();
};

/** Direct-SBI call counts for diagnostics and profiling. */
RV64Debug.prototype.virtSbiCallCounts = function () {
  const names = ["total", "base", "time", "ipi", "rfence", "hsm", "srst", "other"];
  return Object.fromEntries(
    names.map((name, index) => [name, this.ex.virt_sbi_call_count(index)]),
  );
};

/** Current modern-machine PC. Diagnostic API; not part of the stable facade. */
RV64Debug.prototype.virtPc = function () {
  return this.ex.virt_pc();
};

// The browser runtime does not translate HTTP or expose a second transport.
// Linux owns the complete network stack; the only browser boundary is the
// Ethernet frame channel below.

const PUBLIC_EVENTS = new Set([
  "ready",
  "start",
  "stop",
  "error",
  "console",
  "diskError",
  "networkTransmit",
  "downloadProgress",
]);

async function imageBytes(source, name, emit) {
  if (source instanceof Uint8Array) return source;
  if (ArrayBuffer.isView(source)) {
    return new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
  }
  if (source instanceof ArrayBuffer) return new Uint8Array(source);

  let response;
  if (source instanceof Response) response = source;
  else if (source && typeof source === "object" && typeof source.url === "string") {
    response = await fetch(source.url);
  } else {
    throw new TypeError(`${name} must be an ImageSource`);
  }
  if (!response.ok) throw new Error(`${name}: ${response.status} ${response.statusText}`);
  const total = response.headers.has("content-encoding")
    ? undefined
    : Number(response.headers.get("content-length")) || undefined;
  if (!response.body) {
    const bytes = new Uint8Array(await response.arrayBuffer());
    emit("downloadProgress", { image: name, loaded: bytes.length, total });
    return bytes;
  }
  const reader = response.body.getReader();
  const chunks = [];
  let loaded = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    loaded += value.length;
    emit("downloadProgress", { image: name, loaded, total });
  }
  const bytes = new Uint8Array(loaded);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.length;
  }
  return bytes;
}

const hostYieldQueue = [];
let hostYieldChannel;

function hostYield(callback) {
  if (typeof setImmediate === "function") return setImmediate(callback);
  if (typeof MessageChannel === "function") {
    if (!hostYieldChannel) {
      hostYieldChannel = new MessageChannel();
      hostYieldChannel.port1.onmessage = () => hostYieldQueue.shift()?.();
    }
    hostYieldQueue.push(callback);
    hostYieldChannel.port2.postMessage(0);
    return;
  }
  return setTimeout(callback, 0);
}

const RAW_ETHERNET_MAX_FRAME_SIZE = 1_600;
const RAW_ETHERNET_MAX_QUEUED_FRAMES = 256;
const RAW_ETHERNET_MAX_BUFFERED_BYTES =
  RAW_ETHERNET_MAX_FRAME_SIZE * RAW_ETHERNET_MAX_QUEUED_FRAMES;

class RawEthernetWebSocket {
  #socket;
  #pending = [];
  #incoming = [];
  #drainingIncoming = false;
  #pumpTimer = null;
  #closed = false;
  #failed = false;
  #opened = false;
  #onFrame;
  #onFailure;

  constructor(url, protocols, onFrame, onFailure) {
    this.#onFrame = onFrame;
    this.#onFailure = onFailure;
    const socket = new WebSocket(url, protocols);
    socket.binaryType = "arraybuffer";
    socket.onopen = () => {
      this.#opened = true;
      this.#pump();
    };
    socket.onmessage = (event) => {
      if (this.#incoming.length === RAW_ETHERNET_MAX_QUEUED_FRAMES) {
        this.#fail(new Error("raw Ethernet WebSocket receive queue is full"));
        return;
      }
      this.#incoming.push(event.data);
      this.#drainIncoming();
    };
    // The WebSocket API exposes the useful close status only in `close`.
    // Closing here would discard it and collapse every failure into one
    // unactionable message.
    socket.onerror = () => {};
    socket.onclose = (event) => {
      if (!this.#closed) {
        const phase = this.#opened ? "closed unexpectedly" : "failed to connect";
        const detail = event.reason ? `: ${event.reason}` : ` (code ${event.code})`;
        this.#fail(new Error(`raw Ethernet WebSocket ${phase}${detail}`));
      }
    };
    this.#socket = socket;
  }

  send(frame) {
    if (this.#closed) return;
    if (frame.length === 0 || frame.length > RAW_ETHERNET_MAX_FRAME_SIZE) {
      this.#fail(new Error(`guest emitted a ${frame.length}-byte Ethernet frame`));
      return;
    }
    if (this.#pending.length === RAW_ETHERNET_MAX_QUEUED_FRAMES) {
      this.#fail(new Error("raw Ethernet WebSocket transmit queue is full"));
      return;
    }
    this.#pending.push(frame);
    this.#pump();
  }

  close() {
    if (this.#closed) return;
    this.#closed = true;
    this.#pending.length = 0;
    this.#incoming.length = 0;
    if (this.#pumpTimer !== null) clearTimeout(this.#pumpTimer);
    this.#pumpTimer = null;
    const socket = this.#socket;
    socket.onopen = null;
    socket.onmessage = null;
    socket.onerror = null;
    socket.onclose = null;
    socket.close();
  }

  #drainIncoming() {
    if (this.#closed || this.#drainingIncoming) return;
    this.#drainingIncoming = true;
    const consume = (bytes) => {
      const frame = new Uint8Array(bytes);
      if (frame.length === 0 || frame.length > RAW_ETHERNET_MAX_FRAME_SIZE) {
        this.#fail(new Error(`raw Ethernet WebSocket received a ${frame.length}-byte frame`));
        return;
      }
      this.#onFrame(frame);
    };
    const next = () => {
      if (this.#closed) {
        this.#incoming.length = 0;
        this.#drainingIncoming = false;
        return;
      }
      const data = this.#incoming.shift();
      if (data === undefined) {
        this.#drainingIncoming = false;
        return;
      }
      if (typeof data === "string") {
        this.#fail(new Error("raw Ethernet WebSocket received a text message"));
        this.#drainingIncoming = false;
        return;
      }
      if (typeof Blob !== "undefined" && data instanceof Blob) {
        data.arrayBuffer().then((bytes) => {
          consume(bytes);
          next();
        }, (error) => {
          this.#drainingIncoming = false;
          this.#fail(error);
        });
      } else {
        consume(data);
        next();
      }
    };
    next();
  }

  #pump() {
    if (this.#closed || this.#socket.readyState !== WebSocket.OPEN) return;
    while (
      this.#pending.length !== 0 &&
      this.#socket.bufferedAmount <= RAW_ETHERNET_MAX_BUFFERED_BYTES
    ) {
      this.#socket.send(this.#pending.shift());
    }
    if (this.#pending.length !== 0 && this.#pumpTimer === null) {
      this.#pumpTimer = setTimeout(() => {
        this.#pumpTimer = null;
        this.#pump();
      }, 1);
    }
  }

  #fail(error) {
    if (this.#closed || this.#failed) return;
    this.#failed = true;
    this.#onFailure(error);
    this.close();
  }
}

function normalizeNetwork(network) {
  const value = network ?? { mode: "none" };
  if (!value || typeof value !== "object") throw new TypeError("network must be an object");
  if (!["none", "wsproxy", "external"].includes(value.mode)) {
    throw new TypeError(`unknown network mode: ${value.mode}`);
  }
  if (value.mode === "wsproxy" && typeof value.url !== "string") {
    throw new TypeError("wsproxy networking requires url");
  }
  if (value.mac !== undefined && (!(value.mac instanceof Uint8Array) || value.mac.length !== 6)) {
    throw new TypeError("network.mac must be a 6-byte Uint8Array");
  }
  return { ...value };
}

/** Stable embedding API. Raw Wasm and instruction slicing stay private. */
export class RV64 {
  #core;
  #boot;
  #listeners = new Map();
  #running = false;
  #destroyed = false;
  #generation = 0;
  #runSlice;
  #input;
  #instructions;
  #networkConfig;
  #networkInput;
  #rawEthernet;
  #disk;
  #diskOperation = null;

  constructor(core, boot, network, listeners, disk = null) {
    this.#core = core;
    this.#boot = boot;
    this.#networkConfig = network;
    this.#disk = disk;
    for (const [event, listener] of Object.entries(listeners ?? {})) {
      this.on(event, listener);
    }
    this.console = Object.freeze({ send: (data) => this.#sendConsole(data) });
    this.network = Object.freeze({
      mode: network.mode,
      receive: (frame) => this.#receiveNetwork(frame),
    });
  }

  /** Resolve images, instantiate Wasm, and assemble a stopped machine. */
  static async create(options) {
    if (!options || typeof options !== "object") {
      throw new TypeError("RV64.create expects an options object");
    }
    if (options.execution?.mode === "worker") {
      return RV64WorkerProxy.create(options);
    }
    if (options.execution?.mode !== undefined && options.execution.mode !== "local") {
      throw new TypeError(`unknown execution mode: ${options.execution.mode}`);
    }
    const { wasm, boot, memoryMB, events, jit } = options;
    if (!wasm) throw new TypeError("RV64.create requires wasm");
    if (!boot?.mode) throw new TypeError("RV64.create requires boot.mode");

    // Register creation-time listeners before fetching any image.
    const pending = new Map();
    for (const [event, listener] of Object.entries(events ?? {})) {
      if (!PUBLIC_EVENTS.has(event) || typeof listener !== "function") {
        throw new TypeError(`invalid ${event} event listener`);
      }
      pending.set(event, new Set([listener]));
    }
    const emit = (event, detail) => {
      for (const listener of pending.get(event) ?? []) listener(detail);
    };

    const wasmBytes = await imageBytes(wasm, "wasm", emit);
    const resolved = { ...boot };
    for (const key of ["firmware", "kernel", "initrd", "disk"]) {
      const source = boot[key];
      if (source !== undefined) {
        resolved[key] = key === "disk" && isNativeDisk(source)
          ? source
          : await imageBytes(source, key, emit);
      }
    }
    const disk = isNativeDisk(resolved.disk) ? await NativeDiskClient.create(resolved.disk) : null;
    if (disk) resolved.disk = undefined;
    const network = normalizeNetwork(options.network);
    const core = await RV64Debug.create(wasmBytes, jit);
    const vm = new RV64(core, { ...resolved, memoryMB }, network, events, disk);
    vm.#assemble();
    vm.#emit("ready", undefined);
    return vm;
  }

  get running() {
    return this.#running;
  }

  get instructions() {
    this.#assertLive();
    return this.#instructions();
  }

  jitMetrics() {
    this.#assertLive();
    return this.#core.jitMetrics();
  }

  on(event, listener) {
    if (!PUBLIC_EVENTS.has(event)) throw new TypeError(`unknown event: ${event}`);
    if (typeof listener !== "function") throw new TypeError("listener must be a function");
    let listeners = this.#listeners.get(event);
    if (!listeners) this.#listeners.set(event, (listeners = new Set()));
    listeners.add(listener);
    return () => listeners.delete(listener);
  }

  async start() {
    this.#assertLive();
    if (this.#running) return;
    this.#running = true;
    const generation = ++this.#generation;
    this.#emit("start", undefined);
    hostYield(() => this.#tick(generation));
  }

  async stop() {
    this.#assertLive();
    if (this.#running) {
      this.#running = false;
      ++this.#generation;
      this.#emit("stop", { reason: "requested" });
    }
    await this.#waitForDiskOperation();
  }

  async reset() {
    this.#assertLive();
    const restart = this.#running;
    if (restart) await this.stop();
    this.#assemble();
    this.#emit("ready", undefined);
    if (restart) await this.start();
  }

  async destroy() {
    if (this.#destroyed) return;
    await this.stop();
    this.#destroyed = true;
    ++this.#generation;
    this.#rawEthernet?.close();
    this.#rawEthernet = null;
    this.#core.destroyJit();
    this.#listeners.clear();
    this.#core = null;
    this.#disk?.destroy();
    this.#disk = null;
  }

  #assemble() {
    const boot = this.#boot;
    const memoryMB = boot.memoryMB;
    const network = this.#networkConfig;
    const net = network.mode !== "none";
    const networkOptions = {
      net,
      netMac: network.mac,
    };
    const modernCmdline = `${boot.cmdline ?? "console=ttyS0 root=/dev/vda rw"} rv64.network=${network.mode}`;
    if (boot.mode === "firmware") {
      if (!(boot.firmware instanceof Uint8Array)) {
        throw new TypeError("firmware mode requires an OpenSBI image");
      }
      this.#core.bootVirtLinux({
        opensbi: boot.firmware,
        kernel: boot.kernel,
        initrd: boot.initrd,
        disk: boot.disk,
        externalDiskSize: this.#disk?.size,
        cmdline: modernCmdline,
        ramMB: memoryMB ?? 512,
        ...networkOptions,
      });
      this.#runSlice = () => this.#core.virtRunSystemOutcome(2_000_000n);
      this.#input = (bytes) => this.#core.virtConsoleInput(bytes);
      this.#networkInput = (frame) => this.#core.virtNetInput(frame);
      this.#instructions = () => this.#core.virtInsnCount();
    } else if (boot.mode === "linux-direct") {
      if (!(boot.kernel instanceof Uint8Array)) {
        throw new TypeError("linux-direct mode requires a kernel image");
      }
      this.#core.bootVirtLinuxDirect({
        kernel: boot.kernel,
        initrd: boot.initrd,
        disk: boot.disk,
        externalDiskSize: this.#disk?.size,
        cmdline: modernCmdline,
        ramMB: memoryMB ?? 512,
        ...networkOptions,
      });
      this.#runSlice = () => {
        const poweredOff = this.#core.virtRunSystemOutcome(2_000_000n);
        const ext = this.#core.ex.virt_unsupported_sbi_ext();
        if (ext !== 0n) {
          const fn = this.#core.ex.virt_unsupported_sbi_function();
          throw new Error(`unsupported SBI call extension=${ext.toString(16)} function=${fn}`);
        }
        return poweredOff;
      };
      this.#input = (bytes) => this.#core.virtConsoleInput(bytes);
      this.#networkInput = (frame) => this.#core.virtNetInput(frame);
      this.#instructions = () => this.#core.virtInsnCount();
    } else {
      throw new TypeError(`unknown boot mode: ${boot.mode}`);
    }
    this.#core.onWrite = (_fd, bytes) => this.#emit("console", bytes);
    this.#core.onNetSend = (frame) => this.#transmitNetwork(frame);
    this.#connectNetwork();
  }

  async #tick(generation) {
    if (!this.#running || generation !== this.#generation) return;
    try {
      const outcome = this.#runSlice();
      if (outcome === 2) {
        const operation = this.#serviceDisk();
        this.#diskOperation = operation;
        try {
          await operation;
        } finally {
          if (this.#diskOperation === operation) this.#diskOperation = null;
        }
        if (!this.#running || generation !== this.#generation) return;
      } else if (outcome === 1) {
        this.#running = false;
        this.#emit("stop", { reason: "powered-off" });
        return;
      }
      hostYield(() => { void this.#tick(generation); });
    } catch (error) {
      this.#running = false;
      this.#emit("error", error);
      this.#emit("stop", { reason: "error" });
    }
  }

  async #serviceDisk() {
    if (!this.#disk) throw new Error("VM requested native disk I/O without a disk service");
    const request = this.#core.virtDiskRequest();
    if (!request) throw new Error("VM reported disk I/O without a request");
    await serviceNativeDiskRequest(
      this.#core,
      this.#disk,
      request,
      (detail) => this.#emit("diskError", detail),
    );
  }

  async #waitForDiskOperation() {
    const operation = this.#diskOperation;
    if (operation) await operation;
  }

  #sendConsole(data) {
    this.#assertLive();
    if (!this.#input) throw new Error("this boot mode has no console input");
    const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
    if (!(bytes instanceof Uint8Array)) throw new TypeError("console data must be a string or Uint8Array");
    this.#input(bytes);
  }

  #connectNetwork() {
    const network = this.#networkConfig;
    this.#rawEthernet?.close();
    this.#rawEthernet = null;
    if (network.mode === "wsproxy") {
      this.#rawEthernet = new RawEthernetWebSocket(
        network.url,
        network.protocols,
        (frame) => this.#networkInput?.(frame),
        (error) => {
          if (this.#running) {
            this.#running = false;
            ++this.#generation;
            this.#emit("error", error);
            this.#emit("stop", { reason: "error" });
          } else {
            this.#emit("error", error);
          }
        },
      );
    }
  }

  #transmitNetwork(frame) {
    this.#rawEthernet?.send(frame);
    this.#emit("networkTransmit", frame);
  }

  #receiveNetwork(frame) {
    this.#assertLive();
    if (this.#networkConfig.mode !== "external") {
      throw new Error("network.receive is only available in external mode");
    }
    const bytes = frame instanceof Uint8Array ? frame : new Uint8Array(frame);
    this.#networkInput(bytes);
  }

  #emit(event, detail) {
    for (const listener of [...(this.#listeners.get(event) ?? [])]) listener(detail);
  }

  #assertLive() {
    if (this.#destroyed) throw new Error("RV64 instance has been destroyed");
  }
}

class RV64WorkerProxy {
  #worker;
  #listeners = new Map();
  #pending = new Map();
  #nextRequest = 1;
  #running = false;
  #instructions = 0n;
  #jitMetrics = null;
  #statisticsIntervalMs = 500;
  #statisticsRequestPending = false;
  #statisticsTimer = null;
  #destroyed = false;
  #networkMode;

  constructor(worker, networkMode, listeners) {
    this.#worker = worker;
    this.#networkMode = networkMode;
    for (const [event, listener] of Object.entries(listeners ?? {})) {
      this.on(event, listener);
    }
    this.console = Object.freeze({ send: (data) => this.#sendConsole(data) });
    this.network = Object.freeze({
      mode: networkMode,
      receive: (frame) => this.#receiveNetwork(frame),
    });
  }

  static async create(options) {
    if (typeof Worker === "undefined") {
      throw new Error("worker execution requires a browser Worker implementation");
    }
    const execution = options.execution;
    const workerURL = execution.workerURL
      ? new URL(execution.workerURL, import.meta.url)
      : new URL("./rv64.worker.js", import.meta.url);
    // Validate and copy transferable inputs before allocating a Worker so a
    // rejected source (notably Response) cannot leave an idle thread behind.
    const { options: clonedOptions, diagnostics, transfers } = cloneWorkerOptions(options);
    const worker = new Worker(workerURL, { name: "lish-vm", type: "module" });
    const networkMode =
      options.network?.mode ?? "none";
    const proxy = new RV64WorkerProxy(worker, networkMode, options.events);
    const created = new Promise((resolve, reject) => {
      let settled = false;
      const fail = (error) => {
        if (settled) {
          proxy.#fail(error);
        } else {
          settled = true;
          worker.terminate();
          reject(error);
        }
      };
      worker.onerror = (event) =>
        fail(event.error ?? new Error(event.message || "rv64 Worker failed"));
      worker.onmessageerror = () => fail(new Error("rv64 Worker sent an unreadable message"));
      worker.onmessage = (event) => {
        if (event.data?.type === "created") {
          settled = true;
          resolve(event.data.state);
        } else if (event.data?.type === "create-error") {
          fail(deserializeWorkerError(event.data.error));
        } else proxy.#handleMessage(event.data);
      };
    });
    worker.postMessage({
      type: "create",
      eventNames: Object.keys(options.events ?? {}),
      diagnostics,
      options: clonedOptions,
    }, transfers);
    try {
      proxy.#applyState(await created);
      const interval = Number(execution.statisticsIntervalMs);
      proxy.#statisticsIntervalMs =
        Number.isFinite(interval) && interval >= 50 ? interval : 500;
      proxy.#scheduleStatistics();
      return proxy;
    } catch (error) {
      proxy.#destroyed = true;
      worker.terminate();
      throw error;
    }
  }

  get running() {
    return this.#running;
  }

  get instructions() {
    this.#assertLive();
    return this.#instructions;
  }

  jitMetrics() {
    this.#assertLive();
    return this.#jitMetrics;
  }

  on(event, listener) {
    if (!PUBLIC_EVENTS.has(event)) throw new TypeError(`unknown event: ${event}`);
    if (typeof listener !== "function") throw new TypeError("listener must be a function");
    let listeners = this.#listeners.get(event);
    if (!listeners) this.#listeners.set(event, (listeners = new Set()));
    listeners.add(listener);
    return () => listeners.delete(listener);
  }

  async start() {
    this.#assertLive();
    await this.#call("start");
  }

  async stop() {
    this.#assertLive();
    await this.#call("stop");
  }

  async reset() {
    this.#assertLive();
    await this.#call("reset");
  }

  async destroy() {
    if (this.#destroyed) return;
    this.#destroyed = true;
    try {
      await this.#call("destroy", undefined, true);
    } finally {
      clearTimeout(this.#statisticsTimer);
      this.#statisticsTimer = null;
      this.#worker.terminate();
      this.#listeners.clear();
      for (const { reject } of this.#pending.values()) reject(new Error("RV64 instance destroyed"));
      this.#pending.clear();
    }
  }

  #call(method, value, allowDestroyed = false) {
    if (!allowDestroyed) this.#assertLive();
    const id = this.#nextRequest++;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { reject, resolve });
      this.#worker.postMessage({ id, method, type: "call", value });
    });
  }

  #handleMessage(message) {
    if (!message || typeof message !== "object") return;
    if (message.type === "event") {
      if (message.event === "start") this.#running = true;
      if (message.event === "stop") this.#running = false;
      const detail = message.event === "error" ? deserializeWorkerError(message.detail) : message.detail;
      this.#emit(message.event, detail);
      return;
    }
    if (message.type === "state") {
      this.#statisticsRequestPending = false;
      this.#applyState(message.state);
      this.#scheduleStatistics();
      return;
    }
    if (message.type === "result") {
      const pending = this.#pending.get(message.id);
      if (!pending) return;
      this.#pending.delete(message.id);
      this.#applyState(message.state);
      if (message.error) pending.reject(deserializeWorkerError(message.error));
      else pending.resolve(message.value);
      return;
    }
  }

  #applyState(state) {
    if (!state) return;
    this.#running = state.running;
    this.#instructions = BigInt(state.instructions);
    this.#jitMetrics = state.jitMetrics ?? null;
  }

  #scheduleStatistics() {
    if (
      this.#destroyed ||
      this.#statisticsRequestPending ||
      this.#statisticsTimer !== null
    ) return;
    this.#statisticsTimer = setTimeout(() => {
      this.#statisticsTimer = null;
      if (this.#destroyed) return;
      this.#statisticsRequestPending = true;
      this.#worker.postMessage({ type: "state-request" });
    }, this.#statisticsIntervalMs);
  }

  #sendConsole(data) {
    this.#assertLive();
    const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
    if (!(bytes instanceof Uint8Array)) {
      throw new TypeError("console data must be a string or Uint8Array");
    }
    this.#worker.postMessage({ type: "console", value: bytes });
  }

  #receiveNetwork(frame) {
    this.#assertLive();
    if (this.#networkMode !== "external") {
      throw new Error("network.receive is only available in external mode");
    }
    const bytes = frame instanceof Uint8Array ? frame : new Uint8Array(frame);
    this.#worker.postMessage({ type: "network-receive", value: bytes });
  }

  #emit(event, detail) {
    for (const listener of [...(this.#listeners.get(event) ?? [])]) listener(detail);
  }

  #fail(error) {
    if (this.#destroyed) return;
    this.#running = false;
    clearTimeout(this.#statisticsTimer);
    this.#statisticsTimer = null;
    this.#statisticsRequestPending = false;
    this.#emit("error", error);
    this.#emit("stop", { reason: "error" });
    for (const { reject } of this.#pending.values()) reject(error);
    this.#pending.clear();
    this.#worker.terminate();
  }

  #assertLive() {
    if (this.#destroyed) throw new Error("RV64 instance has been destroyed");
  }
}

function cloneWorkerOptions(options) {
  const transfers = [];
  const cloneImage = (source, name) => {
    if (source === undefined) return source;
    if (source instanceof Response) {
      throw new TypeError(`${name} cannot be a Response in worker execution mode`);
    }
    if (source && typeof source === "object" && typeof source.url === "string") {
      const base = globalThis.location?.href ?? import.meta.url;
      return { url: new URL(source.url, base).href };
    }
    let bytes;
    if (source instanceof ArrayBuffer) bytes = new Uint8Array(source);
    else if (ArrayBuffer.isView(source)) {
      bytes = new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
    } else {
      throw new TypeError(`${name} must be bytes or { url }`);
    }
    const copy = bytes.slice();
    transfers.push(copy.buffer);
    return copy;
  };

  const boot = { ...options.boot };
  for (const key of ["firmware", "kernel", "initrd", "disk"]) {
    if (!(key in boot)) continue;
    if (key === "disk" && isNativeDisk(boot[key])) {
      boot[key] = { ...boot[key] };
    } else {
      boot[key] = cloneImage(boot[key], key);
    }
  }
  const network = options.network
    ? {
        ...options.network,
        ...(options.network.mac
          ? { mac: cloneImage(options.network.mac, "network.mac") }
          : {}),
      }
    : undefined;
  return {
    options: {
      wasm: cloneImage(options.wasm, "wasm"),
      boot,
      memoryMB: options.memoryMB,
      ...(options.jit ? { jit: { ...options.jit } } : {}),
      ...(network ? { network } : {}),
      execution: { mode: "local" },
    },
    diagnostics: globalThis.LISH_DIAGNOSTICS === true,
    transfers,
  };
}

function deserializeWorkerError(value) {
  const error = new Error(value?.message ?? String(value));
  if (value?.name) error.name = value.name;
  if (value?.stack) error.stack = value.stack;
  return error;
}
