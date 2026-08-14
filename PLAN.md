# Lish Development Plan

Status: in progress

Last updated: 2026-08-14

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
- an AppKit macOS application with one visible `WKWebView`;
- an authenticated loopback asset server with bundled xterm.js;
- a native `NSTextInputClient` responder for committed text, marked text,
  hardware keys, and terminal edit commands;
- an external virtio-blk backend with pending read, write, and flush requests;
- an authenticated native disk HTTP service backed by ordered `pread`,
  `pwrite`, and `fsync` operations;
- a 64 KiB-line, 64 MiB clean disk cache in the Worker;
- clone-or-copy creation of `vms/default/disk.img` from the Alpine base image;
- application-termination quiesce and cold page recovery after Web content
  process termination;
- low-rate VM state, instruction-rate, and pending-JIT telemetry in the macOS
  title bar;
- successful Safari and Chromium tests for DHCP, DNS, HTTPS, `apk update`,
  and package installation.

The macOS application is a working vertical slice, not a completed product
milestone. Automated tests cover the native service boundaries and the
external disk request state machine. IME behavior, terminal applications,
lossless output backpressure, cold-restart durability, long transfers, and
long-running WebKit memory behavior still need product-level validation.

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
       loopback data services
              |
              v
Swift host
  - asset HTTP listener on one random port
  - disk HTTP listener on one random port
  - raw Ethernet WebSocket listener on one random port
  - libslirp and bounded frame queues
  - ordered disk file I/O
  - lifecycle coordination
```

Keep terminal output in the page. Do not send each output byte through Swift.
Use the loopback services for network frames and disk blocks. Use the page
bridge only for input, control, focus, selection, and telemetry. Do not use
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

## Milestone 2: Validate JIT Bounds (Validation Pending)

The JIT implementation uses asynchronous compilation and a bounded
`JitCodeStore`. Unit and integration tests cover slot reuse, reservation
rollback, invalidation, and destruction. The runtime also exposes an
interpreter-only comparison mode and records pending JIT work in the macOS
application and HTTPS benchmark. Complete the long-running WebKit validation
before changing the ownership architecture.

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

## Milestone 3: Complete Native Networking (Partial)

The product app now creates one authenticated raw Ethernet WebSocket listener
for its VM. The listener is local-only, accepts one active VM session, uses
bounded libslirp queues, and rejects invalid Origin and subprotocol values.
Complete suspend/resume behavior, SSH product controls, and long-duration
tests.

Tasks:

1. Add explicit link-down and link-up handling for suspend and resume.
2. Add a random loopback TCP forward to guest port 22 when SSH is enabled.
3. Add a 1 GiB file transfer test.
4. Measure queue depth and drops during long downloads.
5. Expose network statistics to application diagnostics.

Exit criteria:

- the guest obtains its address through DHCP;
- guest DNS, TCP, UDP, and TLS work without protocol translation;
- `apk update` and package installation complete repeatedly;
- SSH and a 1 GiB transfer work without corruption;
- all frame queues stay bounded;
- suspend and resume produce a clear guest link transition.

## Milestone 4: Build the macOS Application Shell (Vertical Slice Implemented)

The repository contains an AppKit application target and a `tools/build-macos`
packaging script. The application loads its bundled runtime into one visible
Web view and runs the VM in one dedicated Worker.

Implemented:

1. Serve bundled page assets from a capability-bearing loopback URL.
2. Bundle xterm.js, its fit add-on, and all executable Web assets.
3. Connect xterm.js directly to Worker console events.
4. Use a native first responder for committed text, marked text, Ctrl, Return,
   Delete, Esc, Tab, arrows, copy, paste, and selection commands.
5. Disable the xterm browser input path while the native bridge is active.
6. Bound page output to 2 MiB and xterm write credit to 256 KiB.
7. Add Start, Stop, Reset, and Clear Terminal application commands.
8. Show VM state, instruction rate, and pending JIT work in the title bar.
9. Recreate the page and cold-boot the VM after Web content process loss.

Remaining work:

1. Deliver terminal columns and rows to the guest console device.
2. Replace the current overflow drop with lossless execution backpressure.
3. Test IME replacement ranges and Chinese, Japanese, and Korean composition.
4. Test hardware-key ordering, large paste, `nano`, `less`, and shell line
   editing in the packaged application.
5. Verify that repeated page recovery and session destruction release every
   Worker, socket, buffer, and JIT owner.

Exit criteria:

- the app boots to a shell without Web Inspector;
- terminal output updates continuously in WebKit;
- `nano`, `less`, and shell line editing receive correct keys;
- Chinese, Japanese, and Korean composition does not lose or duplicate text;
- large paste and output workloads keep queues bounded;
- destroying a session releases its Worker, sockets, and JIT owners.

## Milestone 5: Add Native Disk Persistence (Implementation Complete)

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

Implemented:

1. Add a pending state to the virtio-blk request state machine.
2. Add range read, range write, and flush operations to the Worker-to-native
   disk protocol.
3. Add native `pread`, `pwrite`, and `fsync` operations on one storage queue.
4. Add a bounded read cache in the Worker.
5. Resume the VM only after the pending operation completes.
6. Create a writable image with clone-or-copy when a VM is created.

The current application supports one persistent VM at
`Application Support/Lish/vms/default/disk.img`. Its random session VM
identifier protects the HTTP route; it is not yet a persistent VM catalog
identifier. JavaScript serializes disk operations, limits each operation to
64 KiB, and invalidates clean cache lines after writes. Swift performs all
file operations on one serial storage queue.

Do not add SQLite, a dirty-block database, a persistent generation number, a
commit log, or a second write-back cache. Do not use NFS or 9P for the root
disk. Add batching only after measurements show that one-operation ordering is
too slow.

Validation still required:

- a disk larger than the cache limit boots successfully;
- package installation survives a cold application restart;
- all writes covered by an acknowledged guest flush survive a process kill;
- cache and pending-request memory stay within configured limits;
- malformed or out-of-range requests fail without modifying the image.

## Milestone 6: Lifecycle and iOS Validation (macOS Partial, iOS Pending)

The macOS application asks the page to stop the VM with a two-second page
deadline. A successful page stop waits for any active disk operation. Swift
then flushes the native disk and stops all three loopback services during
application termination. The application reloads the page and cold-boots from
the same `disk.img` after Web content process termination.

This is a basic termination path. The Worker API still exposes only
`create`, `start`, `stop`, `reset`, and `destroy`; page `quiesce` and `resume`
currently use `stop` and `start` compatibility behavior. Foreground lifecycle
coordination and iOS integration remain pending.

Tasks:

1. Add first-class Worker `quiesce` and `resume` operations.
2. Add deterministic foreground-exit and foreground-entry coordination.
3. Report shutdown timeout or failure instead of treating it as success.
4. Add memory-pressure handling.
5. Build the iOS application, input surface, and accessory key bar.
6. Test lock, background, force quit, repeated Web content termination, and
   low memory.
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

The maintained local suite includes Rust architecture and JIT tests, Worker
API tests, native disk JavaScript tests, and Swift asset, disk, libslirp, and
WebSocket tests. These checks do not replace packaged WebKit application tests
or abrupt-termination durability tests.

The repository is ready for the iOS application milestone only after the JIT,
network, Worker, keyboard, and persistent disk pass their macOS exit criteria.

## Deferred Work

Defer multi-hart support, the RISC-V vector extension, guest RAM snapshots,
multiple concurrent iOS VMs, public inbound services, host directory sharing,
and a native terminal renderer.
