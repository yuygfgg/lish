# Lish

Lish runs a full RV64 Linux system in WebAssembly. The product target is a
macOS and iOS shell application that uses WebKit for WebAssembly compilation.
The first implementation target is macOS.

This repository contains the emulator, browser runtime, Linux image tools,
native host services, tests, and product design. The emulator started from
[`rv64.js`](https://github.com/ibuildthecloud/rv64.js). It is now an internal
Lish subsystem. See [UPSTREAM.md](UPSTREAM.md) for the import record.

Status: pre-release. The WebAssembly full-system machine boots Alpine Linux to
an interactive shell. The current native host connects virtio-net to libslirp
with a raw Ethernet WebSocket. The guest can use DHCP, DNS, TCP, UDP, and TLS.
Native disk streaming and the macOS application shell are not implemented yet.

## Scope

The active runtime has one full-system machine:

- RV64 I, M, A, F, D, C, Zicsr, and privileged execution;
- Sv39 and Sv48 virtual memory;
- direct Linux boot through an emulator-provided SBI;
- virtio block, network, and console devices;
- bounded asynchronous WebAssembly JIT compilation;
- local and dedicated Worker execution;
- raw Ethernet transport over an ordered WebSocket;
- a Swift libslirp host for unprivileged guest networking.

## Local Browser Run

Install the host tools, build the Alpine assets, and build Wasm:

```sh
brew install coreutils genext2fs zig
npm ci
npm run stage-web-dependencies
web/prepare-images.sh
tools/cargo build -p rv64-wasm --target wasm32-unknown-unknown --release
```

Rust commands use the rustup-managed toolchain in `rust-toolchain.toml`.
Repository scripts call `tools/cargo`, which selects that toolchain even when
Homebrew puts another Cargo first in `PATH`.

Start a local file server from the repository root:

```sh
python3 -m http.server 4173
```

Open <http://127.0.0.1:4173/web/>. The page runs the VM in a dedicated Worker
by default. It loads the kernel and disk from `web/images/alpine` and loads the
Wasm module from the repository build output.

The Node harness runs the same machine and forwards the guest serial console:

```sh
node examples/boot-linux.mjs
```

Stop the harness after the Alpine readiness marker:

```sh
RV64_UNTIL=ALPINE_READY node examples/boot-linux.mjs
```

The Alpine image includes the JIT lifecycle benchmark:

```sh
rv64-jit-bench 1024 8 2304
```

## Native Network Host

The Swift package requires libslirp. On macOS, install it with Homebrew:

```sh
brew install libslirp
cd native
swift test
swift run lish-network-host --origin http://127.0.0.1:4173
```

The host prints a WebSocket URL and a capability token. Add both values to the
page URL:

```text
http://127.0.0.1:4173/web/?network=ws://127.0.0.1:PORT/&capability=TOKEN
```

The product service must listen on loopback only. The host has an
`--allow-remote` option for physical-device development. Do not enable that
option in the product application.

The network data path is:

```text
Linux virtio-net
  -> Web Worker
  -> raw Ethernet WebSocket
  -> Swift host
  -> libslirp
  -> host network
```

The guest owns DNS, TCP, UDP, and TLS. Lish does not translate HTTP requests
or install a proxy certificate in the guest.

## JavaScript Runtime

`web/rv64.js` is the product browser runtime. `web/rv64.d.ts` describes its
typed API. New product code must use direct Linux boot, Worker execution, and
raw Ethernet networking:

```js
import { RV64 } from "./rv64.js";

const vm = await RV64.create({
  wasm: { url: "./rv64_wasm.wasm" },
  memoryMB: 512,
  execution: { mode: "worker" },
  boot: {
    mode: "linux-direct",
    kernel: { url: "./Image" },
    disk: { url: "./alpine.ext4" },
    cmdline: "console=ttyS0 root=/dev/vda rw init=/rv64-init",
  },
  network: {
    mode: "wsproxy",
    url: "ws://127.0.0.1:4199/",
    protocols: ["lish.raw-ethernet.v1", capability],
  },
  events: {
    console: bytes => terminal.write(bytes),
    error: error => console.error(error),
  },
});

await vm.start();
vm.console.send("uname -a\n");
await vm.stop();
await vm.destroy();
```

Call `destroy()` when a session ends. Destruction closes transports, cancels
runtime work, and releases JIT ownership. The raw Wasm exports are internal.

The supported Lish path is `linux-direct` with `none`, raw Ethernet `wsproxy`,
or an application-provided external Ethernet transport. The `firmware` mode
remains available when the caller supplies an OpenSBI image. The image builder
does not create or download firmware. The runtime does not expose
user-mode, bare-metal, HTTP proxy, WISP, browser-local LAN, 9P, or secondary
console compatibility APIs.

## Native Full-System Runner

The native runner supports emulator and Linux boot diagnostics:

```sh
tools/cargo build --release --bin rv64-vboot
target/release/rv64-vboot --direct web/images/alpine/Image \
  --disk web/images/alpine/alpine.ext4 --ram 0.5 \
  -- 'console=ttyS0 root=/dev/vda rw init=/rv64-init'
```

The native runner loads its disk into host memory. It is a diagnostic tool, not
the product persistence design.

## Validation

Run the native Rust tests, Wasm build, focused JavaScript tests, and Swift
tests:

```sh
tools/cargo fmt --all -- --check
tools/cargo test --workspace --exclude rv64-wasm --release
tools/cargo test -p rv64-wasm --lib --release
tools/cargo build -p rv64-wasm --target wasm32-unknown-unknown --release
node tests/boot-profile-selftest.mjs
node tests/jit-code-store.mjs
node tests/host-callback-boundary.mjs
node tests/virt-jit.mjs
node tests/public-api.mjs
node tests/worker-api.mjs
node tests/alpine-boot.mjs
(cd native && swift test)
```

Use `tests/run-isa-tests.sh`, `tests/run-arch-tests.sh`, and
`tests/lockstep.py` for architecture validation. These stages need external
RISC-V test sources and reference tools. See
[tests/VALIDATION.md](tests/VALIDATION.md) for the last recorded conformance
results.

Use WebKit as the default engine for browser integration tests. Use Chromium
as a comparison engine. A missing kernel, disk, compiler, or browser is a test
setup failure for a required product gate.

## Repository Layout

- `crates/rv64-core`: CPU, MMU, CSR, and software floating point.
- `crates/rv64-jit`: WebAssembly code emitter.
- `crates/rv64-system`: full-system `virt` machine and devices.
- `crates/rv64-wasm`: plain Wasm host ABI for one full-system VM.
- `web`: Worker runtime and local development page.
- `native`: Swift libslirp bridge and raw Ethernet WebSocket host.
- `kernel`: Linux configuration for the Lish machine.
- `tests`: architecture, runtime, Worker, boot, and native integration tests.
- `tools`: image and benchmark build tools.

The product plan is in [PLAN.md](PLAN.md). The native integration contract is
in [APP_INTEGRATION.md](APP_INTEGRATION.md).

## License

Lish is MIT licensed. See [LICENSE](LICENSE). Retained third-party code and
attribution are listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
