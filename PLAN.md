# Lish Development Plan

Status: in progress

Last updated: 2026-08-13

## Objective

Build a macOS shell application that runs a full RV64 Linux machine in a
visible `WKWebView`. WebKit compiles the emulator and generated guest code as
WebAssembly. xterm.js renders the console. A native input view handles the
system keyboard, IME text, modifiers, and special keys.

The macOS application is the first product milestone. The same runtime and
host protocols must work on iOS after the macOS implementation is stable.

Lish is one repository. The emulator, Web runtime, Swift host, Linux image,
tests, and product documentation change together. Lish does not publish the
emulator as a separate `rv64.js` library.

## Current Baseline

The repository has these working components:

- one RV64 full-system `virt` machine with direct Linux boot;
- a full-system WebAssembly JIT with bounded module and table ownership;
- asynchronous block, batch, and region compilation;
- a dedicated Worker execution mode;
- an Alpine image that contains `rv64-jit-bench`;
- virtio-net frames over a raw Ethernet WebSocket;
- a Swift host that connects the WebSocket to libslirp;
- bounded native frame queues and network statistics;
- native host-forward operations for TCP and UDP;
- successful Safari and Chromium tests for DHCP, DNS, HTTPS, `apk update`,
  and package installation.

An iPhone Safari test also booted the guest and used the native network host.
This result validates the browser runtime on iPhone. It does not validate an
iOS application lifecycle.

The measured Safari run exceeded 250 million guest instructions per second on
one tested workload. Treat that value as a development observation. Record the
browser, operating system, image hash, memory size, and workload for every new
performance result.

## Removed Directions

Do not restore these paths:

- QEMU-Wasm, Emscripten, or Asyncify experiments;
- Linux user-mode emulation and syscall translation;
- TinyEMU source vendoring and a TinyEMU runtime;
- browser HTTP or TLS translation;
- WISP, browser-local LAN, or WANIX integration;
- 9P as the root filesystem;
- a standalone Web library, Pages demo, or release-asset service.

Retain required attribution for source that remains in Lish.

## Product Architecture

```text
Native input and application controls
              |
              v
Visible WKWebView with xterm.js
              |
              v
Dedicated Web Worker
  - Wasm VM and JIT
  - devices and execution loop
  - bounded disk read cache
              |
       loopback HTTP/WebSocket
              |
              v
Swift host service
  - bundled asset delivery
  - libslirp network backend
  - writable disk image
  - lifecycle coordination
```

Keep terminal output in the page. Do not send each output byte through Swift.
Use the loopback service for network frames and disk blocks. Do not use
`WKScriptMessageHandler` for bulk traffic.

## Milestone 1: Repository Convergence (Complete)

The repository now exposes only the full-system machine and its required debug
interfaces.

Completed work:

1. Remove legacy machine and user-mode exports from `rv64-wasm`.
2. Remove HTTP, WISP, 9P, browser-local LAN, and unused boot branches from the
   JavaScript runtime.
3. Keep `linux-direct` as the product boot mode.
4. Keep `none` and raw Ethernet `wsproxy` as the product network modes.
5. Rewrite the test runner and CI for the retained surface.
6. Make required product asset absence fail instead of producing a successful
   skip.
7. Remove release and demo configuration that depends on external asset
   workers.

Result:

- the workspace builds without references to deleted modules;
- no active code references removed runtime features;
- CI runs only maintained tests;
- every required test fails when its required artifact is missing;
- the local Alpine page boots without a hosted release service.

## Milestone 2: Validate JIT Bounds

The JIT implementation already uses asynchronous compilation and a bounded
`JitCodeStore`. Complete the validation before changing its architecture.

Validation tasks:

1. Reboot the guest 100 times in WebKit.
2. Run `rv64-jit-bench` for one hour.
3. Run a self-modifying-code workload.
4. Measure live modules, table slots, emitted bytes, pending compiles, and
   process memory.
5. Confirm that invalidated functions leave the table and become collectible.
6. Confirm that reset and destroy release all JIT owners.

Exit criteria:

- table and module counts reach a stable bound;
- process memory reaches a stable band after warm-up;
- asynchronous compile queues stay within configured limits;
- interpreter and JIT results remain identical;
- guest networking remains responsive during compilation.

Do not add periodic wake-up messages to hide a WebKit scheduling failure.
Find and correct the ownership or scheduling error.

## Milestone 3: Complete Native Networking

The raw Ethernet and libslirp data path works. Complete product controls and
long-duration tests.

Tasks:

1. Keep one authenticated, ordered WebSocket for each VM.
2. Add explicit link-down and link-up handling for suspend and resume.
3. Add a random loopback TCP forward to guest port 22 when SSH is enabled.
4. Add a 1 GiB file transfer test.
5. Measure queue depth and drops during long downloads.
6. Keep the product listener on loopback.

Exit criteria:

- the guest obtains its address through DHCP;
- guest DNS, TCP, UDP, and TLS work without protocol translation;
- `apk update` and package installation complete repeatedly;
- SSH and a 1 GiB transfer work without corruption;
- all frame queues stay bounded;
- suspend and resume produce a clear guest link transition.

## Milestone 4: Build the macOS Application Shell

Create the first product vertical slice before the disk backend. Use a visible
Web view and the current in-memory disk for this milestone.

Tasks:

1. Add a macOS application target.
2. Serve bundled page assets from an authenticated loopback origin.
3. Bundle xterm.js and remove CDN dependencies.
4. Load one visible `WKWebView`.
5. Start the VM in one dedicated Worker.
6. Connect xterm.js to the UART console.
7. Implement native text input, IME composition, Ctrl, Esc, Tab, and arrows.
8. Handle hardware keyboards without duplicate input.
9. Add bounded console output flow control.

Exit criteria:

- the app boots to a shell without Web Inspector;
- terminal output updates continuously in WebKit;
- `nano`, `less`, and shell line editing receive correct keys;
- Chinese, Japanese, and Korean composition does not lose or duplicate text;
- large paste and output workloads keep queues bounded;
- destroying a session releases its Worker, sockets, and JIT owners.

## Milestone 5: Add Native Disk Persistence

Keep virtio-blk as the guest interface. Move the authoritative disk image to
native storage. The Worker must not load the full disk into Wasm memory.

Storage model:

- each VM owns one regular `disk.img` file;
- the file is the only mutable disk state;
- the file size is the virtual disk size;
- the Worker owns a bounded clean read cache;
- version 1 allows one disk operation in flight;
- each guest write reaches `disk.img` before its descriptor completes;
- guest flush maps to ordered native `fsync`.

Implementation tasks:

1. Add a pending state to the virtio-blk request state machine.
2. Add range read, range write, and flush operations to the Worker protocol.
3. Add native `pread`, `pwrite`, and `fsync` operations on one storage queue.
4. Add a bounded read cache in the Worker.
5. Resume the VM only after the pending operation completes.
6. Create a writable image with clone-or-copy when a VM is created.

Do not add SQLite, a dirty-block database, a persistent generation number, a
commit log, or a second write-back cache. Do not use NFS or 9P for the root
disk. Add batching only after measurements show that one-operation ordering is
too slow.

Exit criteria:

- a disk larger than the cache limit boots successfully;
- package installation survives a cold application restart;
- all writes covered by an acknowledged guest flush survive a process kill;
- cache and pending-request memory stay within configured limits;
- malformed or out-of-range requests fail without modifying the image.

## Milestone 6: Lifecycle and iOS Validation

Add deterministic quiesce and cold recovery on macOS. Reuse that contract in a
minimal iOS application.

Tasks:

1. Add `quiesce`, `resume`, and bounded shutdown deadlines.
2. Flush the disk and close networking on foreground exit.
3. Recreate the Worker and cold-boot from `disk.img` after Web content loss.
4. Add memory-pressure handling.
5. Build the iOS input surface and accessory key bar.
6. Test lock, background, force quit, Web content termination, and low memory.
7. Prepare a minimal TestFlight build for App Review risk validation.

Version 1 does not save guest RAM. A terminated Web content process causes a
cold guest boot from the last durable disk state. iOS does not guarantee a
final callback after force quit. Lish must state this limit in product copy.

Exit criteria:

- macOS and iOS use the same Worker, network, and disk protocols;
- foreground quiesce finishes or reports a timeout;
- durable guest writes survive a force quit;
- the listener is not reachable from the local network;
- one-hour WebKit sessions show stable memory and idle CPU use;
- hardware and software keyboards pass the defined input tests.

## Validation Policy

Use WebKit for the default browser integration gate. Use Chromium as a
comparison target. Keep CPU architecture tests separate from product tests.

Every performance result must record:

- host device and operating-system version;
- browser and WebKit version;
- Lish revision and guest image hash;
- guest RAM size;
- workload and elapsed time;
- JIT and process-memory measurements.

The repository is ready for the iOS application milestone only after the JIT,
network, Worker, keyboard, and persistent disk pass their macOS exit criteria.

## Deferred Work

Defer multi-hart support, the RISC-V vector extension, guest RAM snapshots,
multiple concurrent iOS VMs, public inbound services, host directory sharing,
and a native terminal renderer.
