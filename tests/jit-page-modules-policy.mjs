#!/usr/bin/env node
import assert from "node:assert/strict";

const realInstantiate = WebAssembly.instantiate;
const realModule = WebAssembly.Module;
const emptyModule = new Uint8Array([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0]);
let importSequence = 0;

async function configuredPageModules({ pageModules, tailCalls }) {
  let configured;
  WebAssembly.instantiate = async () => ({
    instance: {
      exports: {
        __indirect_function_table: new WebAssembly.Table({
          element: "anyfunc",
          initial: 1,
        }),
        jit_set_page_modules(value) {
          configured = value;
        },
      },
    },
  });
  WebAssembly.Module = class CapabilityProbeModule {
    constructor() {
      if (!tailCalls) {
        throw new WebAssembly.CompileError("tail calls are unavailable");
      }
      return new realModule(emptyModule);
    }
  };

  try {
    const url = new URL(`../web/rv64.js?jit-policy=${importSequence++}`, import.meta.url);
    const { RV64Debug } = await import(url);
    const jit = pageModules === undefined ? {} : { pageModules };
    const vm = await RV64Debug.create(new Uint8Array(), jit);
    vm.destroyJit();
    return configured;
  } finally {
    WebAssembly.instantiate = realInstantiate;
    WebAssembly.Module = realModule;
  }
}

assert.equal(
  await configuredPageModules({ tailCalls: true }),
  1,
  "page modules must default on when the engine supports Wasm tail calls",
);
assert.equal(
  await configuredPageModules({ pageModules: false, tailCalls: true }),
  0,
  "an explicit false must disable page modules",
);
assert.equal(
  await configuredPageModules({ tailCalls: false }),
  0,
  "the default must fall back off when Wasm tail calls are unavailable",
);
assert.equal(
  await configuredPageModules({ pageModules: true, tailCalls: false }),
  0,
  "an unsupported engine must not enable page modules",
);
await assert.rejects(
  configuredPageModules({ pageModules: "yes", tailCalls: true }),
  /jit\.pageModules must be a boolean/,
);

console.log("PASS page-module default and compatibility policy");
