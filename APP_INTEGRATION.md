# Lish Application Integration

Status: macOS vertical slice implemented; product validation in progress

Last updated: 2026-08-14

## Purpose

This document defines the boundary between the Lish WebAssembly VM and the
native macOS or iOS application. It describes the required product path. It
does not define a public emulator library.

The macOS application is the first implementation. The iOS application must
reuse the same page, Worker, network, and disk protocols.

## Fixed Decisions

1. Keep `WKWebView` visible while the terminal is visible.
2. Render the terminal with xterm.js in the page.
3. Run the VM and JIT in one dedicated Web Worker.
4. Use a native input view for text, IME composition, and special keys.
5. Use the guest console as the primary in-app shell transport.
6. Use SSH only for optional external access, transfer, and automation.
7. Carry raw Ethernet frames between the Worker and native libslirp.
8. Store each writable root disk as one native `disk.img` file.
9. Keep only a bounded clean disk cache in the Worker.
10. Cold-boot after Web content process loss. Version 1 does not save RAM.
11. Keep asset, disk, and raw Ethernet parsing in three independently bounded
    loopback services. Use random ports and one shared session capability.

A hidden Web view has no reliable scheduling guarantee. A native-only terminal
would duplicate terminal parsing and move all output through the native bridge.

## Runtime Ownership

```text
Swift main thread
  - application and scene state
  - native input responder
  - Web view ownership

WKWebView page
  - xterm.js rendering
  - terminal selection and scrollback
  - low-rate UI state

Web Worker
  - Wasm instance and guest RAM
  - JIT modules and execution loop
  - virtio devices
  - network WebSocket
  - bounded clean disk cache

Native service queues
  - immutable asset HTTP listener
  - disk HTTP listener and serial file-I/O queue
  - raw Ethernet WebSocket listener and libslirp context
```

No component can call another component's thread-owned state directly. Use
messages or queued operations at every boundary.

## Current Implementation

The repository already contains:

- the full-system Wasm VM and asynchronous JIT;
- Worker lifecycle operations for create, start, stop, reset, and destroy;
- serial console input and output;
- raw Ethernet `wsproxy` transport in the Worker;
- a Swift raw Ethernet WebSocket server;
- a native libslirp wrapper with bounded queues;
- TCP and UDP host-forward operations;
- queue and traffic statistics;
- authentication by Origin, subprotocol name, and capability token;
- a packaged AppKit macOS application with a visible `WKWebView`;
- an authenticated asset server for bundled page, Wasm, kernel, and xterm.js
  files;
- a page runtime that owns xterm.js, VM state, terminal output bounds, and
  low-rate telemetry;
- a native `NSTextInputClient` responder for marked text, committed text,
  special keys, copy, paste, and terminal selection commands;
- an external virtio-blk request state machine with asynchronous read, write,
  and flush completion;
- a native disk service that uses ordered `pread`, `pwrite`, and `fsync`;
- a bounded clean Worker cache with 64 KiB lines and a 64 MiB default limit;
- clone-or-copy creation of one persistent `vms/default/disk.img`;
- application-termination quiesce with a two-second page deadline;
- cold page reload after Web content process termination.

The repository does not yet contain first-class Worker `quiesce` or `resume`
operations, guest terminal-size delivery, explicit network link transitions,
SSH application controls, memory-pressure handling, or an iOS application.
Product-level IME, output backpressure, abrupt-termination durability, and
long-running WebKit tests are also pending.

## Repository Boundary

The repository uses these ownership boundaries:

```text
crates/rv64-core     CPU and architectural state
crates/rv64-jit      WebAssembly guest-code emitter
crates/rv64-system   full-system board and devices
crates/rv64-wasm     Wasm host ABI
web                  Worker runtime and terminal page
native               Swift host libraries and application targets
kernel               guest kernel configuration
tests                cross-boundary validation
```

Do not create a nested emulator repository. A change to a virtio device and
its Swift host protocol belongs in one atomic Lish change.

## Startup

Use this sequence:

1. Swift creates a random session capability.
2. Swift creates a random session VM identifier.
3. Swift validates the required assets.
4. Swift creates `vms/default/disk.img` from the base image if it does not
   exist, then opens and validates the writable image.
5. Swift starts the asset HTTP listener on a random local port and defines the
   page Origin from that port.
6. Swift starts the disk HTTP listener and raw Ethernet WebSocket listener on
   two additional random local ports. The disk service configures CORS for the
   page Origin. The WebSocket requires the exact page Origin.
7. Swift loads the capability-bearing terminal page URL.
8. The page reports `page-ready` through the low-rate bridge.
9. Swift calls `lishBootstrap` with the disk URL, network URL, capability, and
   WebSocket subprotocols.
10. The page creates one Worker. The Worker fetches Wasm and the kernel, reads
    disk geometry through `HEAD`, assembles a stopped VM, and opens the raw
    Ethernet WebSocket.
11. The accepted WebSocket session creates one libslirp context.
12. The page starts the VM and reports its state to Swift.

Report a startup failure at the component that owns it. Do not replace a
failed image, disk, or network operation with an unrelated fallback.

## Loopback Services

The product application uses three `Network.framework` listeners with local
connections only. It generates `127.0.0.1` URLs and random ports. It does not
bind a remotely reachable product endpoint. The development network host can
allow remote access for physical-device tests, but the product application
must not enable that mode.

Use one random capability per application launch. The current endpoints are:

```text
http://127.0.0.1:{asset-port}/s/{capability}/assets/...
http://127.0.0.1:{disk-port}/s/{capability}/vms/{vm-id}/disk
http://127.0.0.1:{disk-port}/s/{capability}/vms/{vm-id}/disk/flush
ws://127.0.0.1:{network-port}/
```

The network URL does not contain the capability. The Worker offers
`lish.raw-ethernet.v1` and the capability as two WebSocket subprotocol values.
The server selects `lish.raw-ethernet.v1` only after it verifies both values
and the exact page Origin.

Use a random session VM identifier in the disk route. Do not treat it as a
persistent catalog identifier. Do not log the capability. Expire all routes
and stop all listeners when the native session ends.

The asset listener limits request headers to 16 KiB and active connections to
eight. The disk listener limits active connections to four and disk operations
to 64 KiB. Each HTTP connection handles one request. The WebSocket server
limits messages to one Ethernet frame and keeps one active VM session.

Serve all executable page assets from the application bundle. Bundle xterm.js
and its styles. Do not load code, fonts, or styles from a CDN.

The current asset response uses this Content Security Policy:

```text
default-src 'none';
script-src 'self' 'wasm-unsafe-eval';
style-src 'self' 'unsafe-inline';
img-src 'self' data:;
connect-src 'self' http://127.0.0.1:* http://[::1]:*
  ws://127.0.0.1:* ws://[::1]:*;
font-src 'self';
object-src 'none';
base-uri 'none';
frame-ancestors 'none'
```

The response also sets `Referrer-Policy: no-referrer` and `Cache-Control:
no-store`. The navigation delegate rejects navigation outside the exact asset
origin and capability path. A native confirmation path for user links is not
implemented.

## Page and Native Control

Use `WKScriptMessageHandler` and `callAsyncJavaScript` for control, native
terminal input, focus, selection, and low-rate telemetry. Network frames, disk
blocks, terminal output, and image data do not cross this bridge.

Each control request has a request identifier and one response. Use these
operations:

```text
create
start
stop
reset
quiesce
resume
destroy
focus-input
console-resize
clear-terminal
```

The page implements all listed operations. The Swift request handler accepts
`focus-input`, the page start notification, `quiesce`, and `destroy`. Swift
invokes page operations with `callAsyncJavaScript`. The page rejects invalid
state transitions and makes `stop`, `quiesce`, and `destroy` idempotent.

`console-resize` currently resizes xterm.js only. Automatic fit events report
columns and rows to Swift, but no native or Worker path delivers them to a
guest console device yet.

## Terminal and Keyboard

The page owns terminal parsing, rendering, selection, and scrollback. The
native view owns platform text input and hardware key events. A terminal tap
asks the native view to become first responder. Do not clear an active terminal
selection when focus changes.

The macOS input responder currently implements these rules:

- send committed text as UTF-8;
- convert Return to carriage return;
- send Delete as `0x7f`;
- preserve marked text until the IME commits it;
- provide Tab, hardware Ctrl, Esc, arrows, copy, paste, and select-all;
- send hardware key events once;
- derive arrow sequences from the terminal cursor-key mode.

The responder stores the marked string and selected range, but automated IME
tests do not yet cover replacement ranges or Chinese, Japanese, and Korean
composition. The iOS input surface must also disable autocorrection,
capitalization, smart quotes, and smart dashes, provide keyboard dismissal and
an accessory row, and support latched and held Ctrl behavior.

For Ctrl with one ASCII character, apply the terminal control mapping after
normalizing the letter to uppercase. Do not apply that mapping to composed
Unicode text.

The extra key bar should follow the useful parts of iSH's design: a native
input responder, a compact accessory row, and explicit hardware key commands.
Implement new Swift code. Do not copy iSH source. iSH uses GPLv3 plus
distribution terms that differ from Lish's MIT license.

Terminal output travels directly from the Worker to the page as Worker events.
It does not cross Swift. The page limits queued output to 2 MiB and starts with
256 KiB of xterm write credit. xterm returns credit through its write
callback. The current overflow path rejects the overflowing chunk, stops the
VM, and restarts after the queue drains. Product completion requires lossless
backpressure before the queue reaches its limit. Transferable output buffers
are not implemented.

The page reports terminal columns and rows after fit or resize. Delivering that
size to the active guest console device remains required. Do not infer
terminal size from pixel dimensions inside Rust.

## Worker Contract

One Worker owns one VM. The Worker currently accepts `create`, `start`, `stop`,
`reset`, and `destroy`. Call messages receive one result; lifecycle and console
notifications use events. The page owns the higher-level application state.

Required state model:

```text
cold -> loading -> stopped -> starting -> running
                    ^                        |
                    +-------- stop ----------+

running -> quiescing -> suspended -> running
any page state -> failed
any live page state -> destroyed
```

Page `quiesce` currently calls Worker `stop`. `stop` prevents new execution
slices and waits for the active disk operation. Swift then calls `fsync` and
stops the network, disk, and asset services. Page `resume` currently maps to
Worker `start`; the macOS application does not yet use it for foreground
lifecycle transitions.

A first-class Worker `quiesce` must stop new execution slices, wait for the
active Wasm entry and disk operation, and return a precise result to the page.
Swift remains responsible for the durable flush, listener shutdown, timeout,
and service recreation.

`destroy` stops execution, waits for disk I/O, closes the WebSocket, clears
timers and listeners, destroys the disk client and Wasm instance, retires JIT
owners, and terminates the Worker proxy.

Keep the Wasm instance, devices, JIT, and socket in the same Worker. Do not
split one VM across multiple Workers.

## Native Network

The product network path is:

```text
guest virtio-net
  -> Worker binary WebSocket
  -> native WebSocket endpoint
  -> libslirp
  -> host network
```

One binary WebSocket message contains one Ethernet frame. The product app uses
the root path on a dedicated random port, so version 1 needs no frame envelope
or network route. Add an envelope only when the data plane needs another
message type.

Reject empty frames and frames larger than 1600 bytes. Keep both native frame
queues bounded. The current default capacity is 256 frames. Expose received,
transmitted, dropped, and queued counts.

libslirp owns DHCP, DNS, ARP, ICMP, TCP, UDP, and NAT behavior. Swift must not
implement those protocols. Use the QEMU user-network defaults unless a test
requires another subnet:

```text
network  10.0.2.0/24
gateway  10.0.2.2
DNS      10.0.2.3
guest    10.0.2.15
```

The guest owns TLS. Do not terminate TLS in JavaScript or Swift. Do not install
a proxy CA.

When SSH is enabled, allocate a random loopback TCP port and forward it to
guest port 22. Keep SSH optional. The console must work before networking or
`sshd` is ready.

Application termination closes the WebSocket and destroys libslirp when Swift
stops the network listener. Explicit suspend/resume is not implemented.
Future suspend must report link-down before closing the socket, then recreate
libslirp and report link-up after reconnect. Existing guest TCP sessions can
fail after suspension.

## Native Disk

### Storage model

The current single-VM application contains one mutable file:

```text
Application Support/Lish/vms/default/disk.img
```

The current application creates `disk.img` from the immutable base image on
the first default-session start. It uses a filesystem clone when available
and a normal copy otherwise. It publishes the file only after image creation
succeeds. A future multi-VM catalog must apply the same operation when the
user creates a VM.

Require a regular, non-empty raw image whose size is a multiple of 512 bytes.
The file size defines the virtual disk size. The protocol needs no database or
disk metadata file.

The native file is the only authoritative mutable state. The Worker owns a
bounded clean read cache with 64 KiB cache lines and a 64 MiB macOS limit.
Both Worker and native service limit one operation to 64 KiB. Select an iOS
cache limit after measuring guest RAM, JIT, and WebKit process memory together.

### Read

The Worker requests one bounded byte range. The native host uses `pread` and
returns that range. A missing or invalid range is an error; the host must not
return the full disk by default.

On a cache miss, virtio-blk keeps the descriptor pending. Rust ends the current
execution slice with a host-I/O reason. The Worker reads the range, supplies
the bytes to Wasm, completes the descriptor, and resumes the VM. This state
machine does not require Asyncify.

The cache contains clean data only. Eviction requires no write. Invalidate all
cached ranges that overlap a completed write.

### Write

The Worker sends an offset and an immutable byte body. The native host rejects
an empty body, an invalid offset, or a range past the disk end. One native
storage queue performs `pwrite` operations in request order.

Version 1 allows one disk operation in flight. Complete the guest descriptor
only after `pwrite` writes the complete body. Do not call `fsync` for each
write. Retrying the same offset and body is idempotent, so the protocol does
not need request generations or a commit log.

### Flush

Advertise `VIRTIO_BLK_F_FLUSH` and implement `VIRTIO_BLK_T_FLUSH`. Keep the
flush descriptor pending. Wait for the active write, call `fsync`, complete the
descriptor, and resume the VM.

Lish guarantees durability only for writes covered by an acknowledged guest
flush. The operating system can preserve later writes, but the product must not
promise those writes after abrupt termination.

Do not add SQLite, a dirty overlay, a persistent generation counter, a custom
journal, NFS, or 9P to this path. Add bounded request batching only after
measurements show that the simple ordered protocol is too slow.

## Lifecycle

The current macOS termination sequence is:

1. Ask the page to quiesce and allow two seconds for the page call.
2. If the page call completes, stop new VM slices and wait for the active disk
   operation.
3. Request a durable native disk flush on the ordered storage queue.
4. Stop the network, disk, and asset listeners.
5. Allow application termination.

The current code does not explicitly disable terminal input or surface a page
deadline or flush failure to the user during termination. Deterministic
lifecycle completion requires explicit success, timeout, and failure results.

On iOS, start a finite background task before quiesce. Use a short deadline.
The background task gives more time but does not guarantee Worker scheduling.

Foreground exit and entry handling is not implemented. If WebKit terminates
the content process while the macOS app remains alive, the app reloads the
page. The existing native services remain active, and the new page creates a
new Worker and cold-boots from `disk.img`.

iOS does not provide a reliable final callback after force quit. A system
memory kill can also skip final cleanup. Do not claim continuous background
execution or guaranteed final persistence.

On memory pressure, request a flush, reduce terminal scrollback, evict cold JIT
owners, and release inactive transfer buffers. Do not allocate a RAM snapshot.

## Metrics

The macOS title bar currently publishes VM state, current instruction rate,
and pending JIT builds. The native network library exposes traffic, drop, and
queue statistics to callers. The application does not yet publish the full
diagnostic snapshot.

Publish the complete low-rate snapshot only while diagnostics are visible.
Include:

- VM state and retired instructions;
- current and average MIPS;
- Wasm linear memory size;
- JIT live modules, slots, bytes, evictions, and pending compiles;
- terminal queued bytes and credit;
- network bytes, frames, drops, and queue depth;
- disk cache bytes and read, write, and flush latency;
- Web content process restarts.

`WebAssembly.Memory.buffer.byteLength` measures Wasm linear memory. It does not
measure total browser or Web content process memory. Use native development
tools for total process measurements. Do not use private memory APIs in the
product.

Logs must not contain terminal contents, guest file data, capability tokens,
or SSH credentials.

## Platform Policy

WebAssembly in `WKWebView` does not guarantee App Store approval. App Review
can evaluate downloaded guest packages and general shell access.

Keep all native executable code in the signed application. Do not download
native frameworks, dynamic libraries, or executable host plug-ins. Guest code
runs inside the emulated RISC-V machine and cannot call iOS APIs directly.

Use a minimal TestFlight build to test policy risk before product work depends
on approval. iSH is a useful precedent, not an approval guarantee.

## Required Tests

Use WebKit as the default browser engine. Add tests for:

- packaged-app boot, serial input, serial output, and Worker destruction;
- IME composition and replacement ranges;
- Ctrl, Alt, Esc, Tab, arrows, and hardware-key ordering;
- large paste with concurrent terminal output;
- Origin and capability rejection;
- Ethernet frame limits, queue bounds, DHCP, DNS, TCP, UDP, and TLS;
- SSH forwarding and a 1 GiB transfer;
- disk range validation, write ordering, retry, and flush durability;
- quiesce timeout, resume, and Web content cold recovery.

Record the operating system, WebKit version, Lish revision, guest image hash,
RAM size, and workload for each performance or memory test.

Current automated coverage includes Worker lifecycle, external virtio-blk
pending requests, disk cache and ordering, disk HTTP authentication and range
validation, disk image creation, large asset delivery, raw
Ethernet authentication and framing, DHCP, ARP, UDP, libslirp forwarding,
cursor focus, and telemetry formatting. It does not satisfy the product-level
tests listed above.
