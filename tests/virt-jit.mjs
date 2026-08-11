#!/usr/bin/env node
// Fast VirtMachine JIT integration test. The synthetic kernels enter directly
// in supervisor mode, so this test does not need firmware, Linux, or a disk.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { RV64Debug } from "../web/rv64.js";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const wasm = await readFile(
  join(root, "target/wasm32-unknown-unknown/release/rv64_wasm.wasm"),
);

const KERNEL_BASE = 0x8020_0000;
const SATP = 0x180;
const SBI_SRST = 0x5352_5354;

const op = {
  addi: 0x13,
  auipc: 0x17,
  branch: 0x63,
  jal: 0x6f,
  jalr: 0x67,
  load: 0x03,
  lui: 0x37,
  op: 0x33,
  store: 0x23,
  system: 0x73,
};

function encodeI(opcode, funct3, rd, rs1, immediate) {
  return (
    opcode |
    (rd << 7) |
    (funct3 << 12) |
    (rs1 << 15) |
    ((immediate & 0xfff) << 20)
  ) >>> 0;
}

function encodeR(opcode, funct3, funct7, rd, rs1, rs2) {
  return (
    opcode |
    (rd << 7) |
    (funct3 << 12) |
    (rs1 << 15) |
    (rs2 << 20) |
    (funct7 << 25)
  ) >>> 0;
}

function encodeS(opcode, funct3, rs1, rs2, immediate) {
  const value = immediate & 0xfff;
  return (
    opcode |
    ((value & 0x1f) << 7) |
    (funct3 << 12) |
    (rs1 << 15) |
    (rs2 << 20) |
    ((value >>> 5) << 25)
  ) >>> 0;
}

function encodeB(funct3, rs1, rs2, offset) {
  const immediate = offset & 0x1fff;
  return (
    op.branch |
    (((immediate >>> 11) & 1) << 7) |
    (((immediate >>> 1) & 0xf) << 8) |
    (funct3 << 12) |
    (rs1 << 15) |
    (rs2 << 20) |
    (((immediate >>> 5) & 0x3f) << 25) |
    (((immediate >>> 12) & 1) << 31)
  ) >>> 0;
}

function encodeU(opcode, rd, immediate) {
  return (opcode | (rd << 7) | ((immediate & 0xfffff) << 12)) >>> 0;
}

function encodeJ(rd, offset) {
  const immediate = offset & 0x1f_ffff;
  return (
    op.jal |
    (rd << 7) |
    (((immediate >>> 12) & 0xff) << 12) |
    (((immediate >>> 11) & 1) << 20) |
    (((immediate >>> 1) & 0x3ff) << 21) |
    (((immediate >>> 20) & 1) << 31)
  ) >>> 0;
}

class KernelBuilder {
  constructor() {
    this.words = [];
    this.labels = new Map();
    this.fixups = [];
  }

  emit(word) {
    this.words.push(word >>> 0);
  }

  label(name) {
    assert(!this.labels.has(name), `duplicate label: ${name}`);
    this.labels.set(name, this.words.length);
  }

  add(rd, rs1, rs2) {
    this.emit(encodeR(op.op, 0, 0, rd, rs1, rs2));
  }

  addi(rd, rs1, immediate) {
    this.emit(encodeI(op.addi, 0, rd, rs1, immediate));
  }

  andi(rd, rs1, immediate) {
    this.emit(encodeI(op.addi, 7, rd, rs1, immediate));
  }

  lbu(rd, rs1, immediate = 0) {
    this.emit(encodeI(op.load, 4, rd, rs1, immediate));
  }

  ld(rd, rs1, immediate = 0) {
    this.emit(encodeI(op.load, 3, rd, rs1, immediate));
  }

  sw(rs2, rs1, immediate = 0) {
    this.emit(encodeS(op.store, 2, rs1, rs2, immediate));
  }

  sb(rs2, rs1, immediate = 0) {
    this.emit(encodeS(op.store, 0, rs1, rs2, immediate));
  }

  sd(rs2, rs1, immediate = 0) {
    this.emit(encodeS(op.store, 3, rs1, rs2, immediate));
  }

  li(rd, value) {
    assert(Number.isInteger(value) && value >= 0 && value <= 0x7fff_ffff);
    const upper = Math.floor((value + 0x800) / 0x1000);
    const lower = value - upper * 0x1000;
    this.emit(encodeU(op.lui, rd, upper));
    if (lower !== 0) this.addi(rd, rd, lower);
  }

  loadAddress(rd, address) {
    const pc = KERNEL_BASE + this.words.length * 4;
    const delta = address - pc;
    const upper = Math.floor((delta + 0x800) / 0x1000);
    const lower = delta - upper * 0x1000;
    this.emit(encodeU(op.auipc, rd, upper));
    this.addi(rd, rd, lower);
  }

  loadLiteral64(rd, address) {
    const pc = KERNEL_BASE + this.words.length * 4;
    const delta = address - pc;
    const upper = Math.floor((delta + 0x800) / 0x1000);
    const lower = delta - upper * 0x1000;
    this.emit(encodeU(op.auipc, rd, upper));
    this.ld(rd, rd, lower);
  }

  bne(rs1, rs2, label) {
    this.fixups.push({ index: this.words.length, kind: "bne", label, rs1, rs2 });
    this.emit(0);
  }

  jal(rd, label) {
    this.fixups.push({ index: this.words.length, kind: "jal", label, rd });
    this.emit(0);
  }

  jalr(rd, rs1, immediate = 0) {
    this.emit(encodeI(op.jalr, 0, rd, rs1, immediate));
  }

  csrw(csr, rs1) {
    this.emit((op.system | (1 << 12) | (rs1 << 15) | (csr << 20)) >>> 0);
  }

  sfenceVma() {
    this.emit(0x1200_0073);
  }

  ecall() {
    this.emit(0x0000_0073);
  }

  sbiReset() {
    this.li(17, SBI_SRST);
    this.addi(16, 0, 0);
    this.addi(10, 0, 0);
    this.ecall();
  }

  finish(size = 0x1000) {
    for (const fixup of this.fixups) {
      const target = this.labels.get(fixup.label);
      assert.notEqual(target, undefined, `unknown label: ${fixup.label}`);
      const offset = (target - fixup.index) * 4;
      if (fixup.kind === "bne") {
        assert(offset >= -4096 && offset < 4096 && offset % 2 === 0);
        this.words[fixup.index] = encodeB(1, fixup.rs1, fixup.rs2, offset);
      } else {
        assert(offset >= -0x10_0000 && offset < 0x10_0000 && offset % 2 === 0);
        this.words[fixup.index] = encodeJ(fixup.rd, offset);
      }
    }
    assert(this.words.length * 4 <= size, "kernel code exceeds image size");
    const image = new Uint8Array(size);
    const view = new DataView(image.buffer);
    this.words.forEach((word, index) => view.setUint32(index * 4, word, true));
    return image;
  }
}

function finishWithFailureLoop(builder, size) {
  builder.sbiReset();
  builder.label("fail");
  builder.jal(0, "fail");
  return builder.finish(size);
}

function hotLoopKernel() {
  const iterations = 1_000_000;
  const b = new KernelBuilder();
  b.li(5, iterations);
  b.addi(6, 0, 0);
  b.label("loop");
  b.addi(6, 6, 1);
  b.addi(5, 5, -1);
  b.bne(5, 0, "loop");
  b.li(7, iterations);
  b.bne(6, 7, "fail");
  return finishWithFailureLoop(b);
}

function tlbRefillKernel() {
  const iterations = 500_000;
  const literalOffset = 0x800;
  const rootOffset = 0x1000;
  const firstDataOffset = 0x2000;
  // The two virtual pages differ by 4096 pages. They alias the direct-mapped
  // JIT TLB, which forces both load and store refill paths after tier-up.
  const secondDataOffset = firstDataOffset + 0x1000_000;
  const imageSize = firstDataOffset + 8;
  const b = new KernelBuilder();

  b.loadLiteral64(5, KERNEL_BASE + literalOffset);
  b.csrw(SATP, 5);
  b.sfenceVma();
  b.loadAddress(11, KERNEL_BASE + firstDataOffset);
  b.loadAddress(12, KERNEL_BASE + secondDataOffset);
  b.li(5, iterations);
  b.label("loop");
  b.ld(6, 11);
  b.addi(6, 6, 1);
  b.sd(6, 11);
  b.ld(7, 12);
  b.addi(7, 7, 1);
  b.sd(7, 12);
  b.addi(5, 5, -1);
  b.bne(5, 0, "loop");
  b.ld(6, 11);
  b.ld(7, 12);
  b.li(8, iterations);
  b.bne(6, 8, "fail");
  b.bne(7, 8, "fail");
  const image = finishWithFailureLoop(b, imageSize);
  const view = new DataView(image.buffer);

  const rootAddress = BigInt(KERNEL_BASE + rootOffset);
  const satp = (8n << 60n) | (rootAddress >> 12n);
  view.setBigUint64(literalOffset, satp, true);

  // One 1 GiB Sv39 leaf maps 0x80000000..0xbfffffff identically. V, R, W,
  // X, A, and D are set so the test isolates the JIT refill ABI.
  const identityPte = ((0x8000_0000n >> 12n) << 10n) | 0x0cfn;
  const rootIndex = 2;
  view.setBigUint64(rootOffset + rootIndex * 8, identityPte, true);
  return image;
}

function asyncStaleKernel() {
  const uart = 0x1000_0000;
  const padding = KERNEL_BASE + 0xc00;
  const b = new KernelBuilder();

  b.li(20, uart);
  b.loadAddress(21, padding);
  b.label("dispatch");
  for (let i = 0; i < 8; i++) b.jal(1, `function${i}`);
  b.lbu(5, 20, 5);
  b.andi(5, 5, 1);
  b.bne(5, 0, "mutate");
  b.jal(0, "dispatch");

  b.label("mutate");
  b.lbu(5, 20);
  b.sw(0, 21);
  b.jal(0, "dispatch");

  for (let i = 0; i < 8; i++) {
    b.label(`function${i}`);
    b.addi(6, 6, i + 1);
    b.jalr(0, 1);
  }
  return b.finish();
}

function outputFloodKernel() {
  const outputBytes = 10_000;
  const b = new KernelBuilder();
  b.li(20, 0x1000_0000);
  b.li(5, outputBytes);
  b.addi(6, 0, 65);
  b.label("output");
  b.sb(6, 20);
  b.addi(5, 5, -1);
  b.bne(5, 0, "output");
  return { image: finishWithFailureLoop(b), outputBytes };
}

function outputThenStopKernel(stopInstruction) {
  const b = new KernelBuilder();
  b.li(20, 0x1000_0000);
  b.addi(6, 0, 65);
  b.sb(6, 20);
  if (stopInstruction === "poweroff") b.sbiReset();
  else b.emit(0x1050_0073); // wfi
  b.label("fail");
  b.jal(0, "fail");
  return b.finish();
}

async function runKernel(kernel, configure = () => {}) {
  const vm = await RV64Debug.create(wasm);
  configure(vm);
  vm.bootVirtLinuxDirect({ kernel, ramMB: 32 });
  let poweredOff = false;
  const instructionLimit = 30_000_000n;
  while (!poweredOff && vm.virtInsnCount() < instructionLimit) {
    poweredOff = vm.runVirtSystem(1_000_000n);
  }
  return { vm, poweredOff };
}

let hotJit;
{
  const { vm, poweredOff } = await runKernel(hotLoopKernel());
  const sbi = vm.virtSbiCallCounts();
  assert.equal(poweredOff, true, "hot-loop kernel did not reach SBI SRST");
  assert.equal(sbi.srst, 1n, "direct SBI reset was not serviced");
  hotJit = {
    retired: vm.ex.jit_stat(0),
    dispatches: vm.ex.jit_stat(1),
    cacheEntries: vm.ex.jit_stat(3),
    registrations: vm.jitBlocks ?? 0,
  };
  vm.destroyJit();
}

let tlbJit;
{
  const { vm, poweredOff } = await runKernel(tlbRefillKernel(), (machine) => {
    machine.ex.jit_set_tlb_fill(1);
  });
  const sbi = vm.virtSbiCallCounts();
  assert.equal(poweredOff, true, "TLB kernel produced a wrong result or did not exit");
  assert.equal(sbi.srst, 1n, "TLB kernel did not exit through direct SBI");
  tlbJit = {
    retired: vm.ex.jit_stat(0),
    refills: vm.ex.jit_stat(31),
  };
  vm.destroyJit();
}

assert.ok(hotJit.retired > 0n, "virt_run retired no JIT instructions");
assert.ok(hotJit.dispatches > 0n, "virt_run dispatched no JIT blocks");
assert.ok(hotJit.cacheEntries > 0n, "VirtMachine populated no system JIT cache entries");
assert.ok(hotJit.registrations > 0, "the host registered no VirtMachine JIT module");
assert.ok(tlbJit.retired > 0n, "TLB kernel never entered compiled code");
assert.ok(
  tlbJit.refills > 0n,
  "compiled VirtMachine memory operations never used the TLB refill ABI",
);

console.log("PASS VirtMachine JIT dispatch, direct SBI, and context-aware TLB refill");

{
  const vm = await RV64Debug.create(wasm);
  const { image, outputBytes } = outputFloodKernel();
  const chunks = [];
  vm.onWrite = (_fd, bytes) => chunks.push(bytes.length);
  vm.bootVirtLinuxDirect({ kernel: image, ramMB: 32 });

  const poweredOff = vm.runVirtSystem(100_000_000n);

  assert.equal(poweredOff, false, "output flood ran to completion in one host turn");
  assert.equal(chunks.length, 1, "one run entry produced multiple host drain batches");
  assert.ok(chunks[0] > 0 && chunks[0] < outputBytes, "host output did not force an early yield");
  vm.destroyJit();
}

console.log("PASS host output forces a bounded full-system run boundary");

for (const [stopInstruction, expectedPowerOff] of [
  ["poweroff", true],
  ["wfi", false],
]) {
  const vm = await RV64Debug.create(wasm);
  let output = "";
  vm.onWrite = (_fd, bytes) => {
    output += new TextDecoder().decode(bytes);
  };
  vm.bootVirtLinuxDirect({
    kernel: outputThenStopKernel(stopInstruction),
    ramMB: 32,
  });

  const poweredOff = vm.runVirtSystem(1_000_000n);

  assert.equal(poweredOff, expectedPowerOff);
  assert.equal(output, "A", `${stopInstruction} lost final UART output`);
  vm.destroyJit();
}

console.log("PASS final host I/O flush preserves power-off and realtime WFI state");

{
  const vm = await RV64Debug.create(wasm);
  const originalCompile = WebAssembly.compile;
  let releaseCompile;
  const compileGate = new Promise((resolve) => {
    releaseCompile = resolve;
  });
  let compileCalls = 0;
  WebAssembly.compile = async function gatedCompile(source) {
    compileCalls++;
    const module = await originalCompile.call(WebAssembly, source);
    await compileGate;
    return module;
  };

  try {
    vm.ex.sys_set_superblock(1);
    vm.ex.jit_set_sb_spacing(0);
    vm.ex.jit_set_batch(0);
    vm.bootVirtLinuxDirect({ kernel: asyncStaleKernel(), ramMB: 32 });

    const issuedBefore = vm.ex.jit_stat(12);
    for (let slice = 0; slice < 100 && vm.ex.sys_pending_builds() === 0; slice++) {
      vm.runVirtSystem(100_000n);
    }
    assert.equal(vm.ex.sys_pending_builds(), 1, "no async superblock build was issued");
    assert.equal(compileCalls, 1, "the superblock did not enter async WebAssembly compilation");
    assert.equal(vm.ex.jit_stat(12), issuedBefore + 1n);

    const beforeMutation = {
      dirty: vm.ex.jit_stat(23),
      dropped: vm.ex.jit_stat(24),
      issued: vm.ex.jit_stat(12),
      registrations: vm.jitBlocks ?? 0,
    };
    vm.virtConsoleInput(Uint8Array.of(1));

    let dirtyDrained = false;
    let pageRecompiled = false;
    for (let step = 0; step < 10_000 && !pageRecompiled; step++) {
      vm.runVirtSystem(1n);
      dirtyDrained ||= vm.ex.jit_stat(23) > beforeMutation.dirty;
      pageRecompiled =
        dirtyDrained && (vm.jitBlocks ?? 0) > beforeMutation.registrations;
    }
    assert.equal(dirtyDrained, true, "the guest code-page write was not drained");
    assert.ok(
      vm.ex.jit_stat(24) > beforeMutation.dropped,
      "dirty-page drain removed no compiled block",
    );
    assert.equal(pageRecompiled, true, "the drained code page was not synchronously recompiled");
    assert.equal(
      vm.ex.jit_stat(12),
      beforeMutation.issued,
      "a second async build obscured the generation under test",
    );
    assert.equal(vm.ex.sys_pending_builds(), 1, "the old async result completed before release");

    const landedBefore = vm.ex.jit_stat(13);
    const staleBefore = vm.ex.jit_stat(14);
    const metricsBefore = vm.jitMetrics();
    releaseCompile();
    for (let turn = 0; turn < 100 && vm.ex.sys_pending_builds() !== 0; turn++) {
      await new Promise((resolve) => setImmediate(resolve));
    }

    const metricsAfter = vm.jitMetrics();
    assert.equal(vm.ex.sys_pending_builds(), 0, "async superblock completion did not drain");
    assert.equal(vm.ex.jit_stat(13), landedBefore, "stale code was installed");
    assert.equal(vm.ex.jit_stat(14), staleBefore + 1n, "generation mismatch was not rejected");
    assert.equal(metricsAfter.liveSlots, metricsBefore.liveSlots, "stale JIT slot leaked");
    assert.equal(metricsAfter.rustLiveSlots, metricsBefore.rustLiveSlots);
    assert.equal(metricsAfter.registeredModules, metricsBefore.registeredModules + 1);
    assert.equal(metricsAfter.retiredSlots, metricsBefore.retiredSlots + 1);
  } finally {
    releaseCompile();
    WebAssembly.compile = originalCompile;
    vm.destroyJit();
  }
}

console.log("PASS async superblock rejects dirty-drain/re-mark ABA result");
