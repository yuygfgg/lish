# Lish Application Integration

Status: design and partial implementation

Last updated: 2026-08-13

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
  - HTTP asset and disk service
  - raw Ethernet WebSocket
  - libslirp context
  - disk file I/O
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
- authentication by Origin, subprotocol name, and capability token.

The repository does not yet contain the macOS application, native asset
server, native disk service, terminal input view, quiesce lifecycle, or Web
content recovery.

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
2. Swift opens the writable VM disk image.
3. Swift starts a loopback HTTP and WebSocket service.
4. Swift creates one libslirp context for the VM.
5. Swift loads the bundled terminal page from the loopback origin.
6. The page starts one Worker.
7. The Worker loads Wasm, the kernel, and initial boot data.
8. The Worker opens the authenticated raw Ethernet WebSocket.
9. The Worker creates the VM in a stopped state.
10. The page reports readiness to Swift.
11. Swift enables input and starts the VM.

Report a startup failure at the component that owns it. Do not replace a
failed image, disk, or network operation with an unrelated fallback.

## Loopback Service

Bind the product listener to `127.0.0.1` and `::1`. Do not bind to
`0.0.0.0`. The development network host can allow remote access for physical
device tests, but the product application must not enable that mode.

Use one random capability per application launch. Put it in each HTTP and
WebSocket route:

```text
/s/{capability}/assets/...
/s/{capability}/vms/{vm-id}/network
/s/{capability}/vms/{vm-id}/disk
/s/{capability}/vms/{vm-id}/disk/flush
```

Use a random VM identifier. Do not log the capability. Expire all routes when
the native session ends.

Validate the exact page Origin on every WebSocket upgrade. Require the
`lish.raw-ethernet.v1` subprotocol and the capability token. Bound HTTP header
size, request size, connection count, WebSocket message size, and pending I/O.

Serve all executable page assets from the application bundle. Bundle xterm.js
and its styles. Do not load code, fonts, or styles from a CDN.

Use a restrictive Content Security Policy:

```text
default-src 'none';
script-src 'self' 'wasm-unsafe-eval';
style-src 'self';
img-src 'self' data:;
connect-src 'self' ws://127.0.0.1:PORT ws://[::1]:PORT;
font-src 'self';
object-src 'none';
base-uri 'none';
frame-ancestors 'none'
```

Add `Referrer-Policy: no-referrer`. Reject navigation away from the loopback
origin. Open user links through a native confirmation path.

## Page and Native Control

Use `WKScriptMessageHandler` or `callAsyncJavaScript` only for low-rate control
messages. Network frames, disk blocks, terminal output, and large image data
must not cross this bridge.

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
```

Reject an operation that is invalid for the current state. Make `stop`,
`quiesce`, and `destroy` idempotent.

## Terminal and Keyboard

The page owns terminal parsing, rendering, selection, and scrollback. The
native view owns platform text input and hardware key events. A terminal tap
asks the native view to become first responder. Do not clear an active terminal
selection when focus changes.

The input surface must implement these rules:

- send committed text as UTF-8;
- convert Return to carriage return;
- send Delete as `0x7f`;
- preserve marked text until the IME commits it;
- support replacement inside the marked range;
- disable autocorrection, capitalization, smart quotes, and smart dashes;
- provide Tab, Ctrl, Esc, arrows, paste, and keyboard dismissal;
- support a latched Ctrl key and a held Ctrl key;
- send hardware key events once;
- derive arrow sequences from the terminal cursor-key mode.

For Ctrl with one ASCII character, apply the terminal control mapping after
normalizing the letter to uppercase. Do not apply that mapping to composed
Unicode text.

The extra key bar should follow the useful parts of iSH's design: a native
input responder, a compact accessory row, and explicit hardware key commands.
Implement new Swift code. Do not copy iSH source. iSH uses GPLv3 plus
distribution terms that differ from Lish's MIT license.

Send terminal output directly from the Worker to the page with transferable
buffers. Bound queued output by byte count. The page returns credit after
xterm.js accepts data. Stop the VM at a bounded execution boundary when credit
runs out.

The page reports terminal columns and rows after fit or resize. The Worker
must deliver the size to the active guest console device. Do not infer terminal
size from pixel dimensions inside Rust.

## Worker Contract

One Worker owns one VM. It receives versioned request messages and returns one
response for each request. Events use separate message types.

Required state model:

```text
cold -> loading -> stopped -> running
                    ^          |
                    +-- stop --+

running -> quiescing -> suspended -> running
any live state -> failed
any live state -> destroyed
```

`quiesce` stops new execution slices, waits for the active Wasm entry to
return, waits for the active disk operation, requests a disk flush, closes the
network socket, and reports success or timeout.

`destroy` closes sockets, cancels requests, clears timers, releases image and
transfer buffers, destroys the Wasm instance, and retires all JIT owners.

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

One binary WebSocket message contains one Ethernet frame. Version 1 needs no
frame envelope because the socket carries only frames. Add an envelope only
when the data plane needs another message type.

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

Close the WebSocket during quiesce. Recreate libslirp and the WebSocket on
resume. Existing guest TCP sessions can fail after suspension. Report a link
transition so the guest does not wait indefinitely for stale connections.

## Native Disk

### Storage model

Each VM directory contains one mutable file:

```text
vm/
  disk.img
```

Create `disk.img` from an immutable base image when the user creates a VM. Use
a filesystem clone when available. Use a normal copy otherwise. Publish the
VM only after image creation succeeds.

Require a regular, non-empty raw image whose size is a multiple of 512 bytes.
The file size defines the virtual disk size. The protocol needs no database or
disk metadata file.

The native file is the only authoritative mutable state. The Worker owns a
bounded clean read cache. Start with 64 KiB cache lines and a 64 MiB macOS
limit. Select an iOS limit after measuring guest RAM, JIT, and WebKit process
memory together.

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

On foreground exit:

1. Disable new terminal input.
2. Ask the Worker to quiesce.
3. Stop scheduling new VM slices.
4. Wait for the active Wasm call.
5. Wait for the active disk operation.
6. Request a durable disk flush.
7. Close the network socket.
8. Report success or timeout to Swift.

On iOS, start a finite background task before quiesce. Use a short deadline.
The background task gives more time but does not guarantee Worker scheduling.

On foreground entry, resume the existing Worker if it is alive. Recreate
libslirp and reconnect networking. If WebKit terminated the content process,
destroy the old session, reload the page, and cold-boot from `disk.img`.

iOS does not provide a reliable final callback after force quit. A system
memory kill can also skip final cleanup. Do not claim continuous background
execution or guaranteed final persistence.

On memory pressure, request a flush, reduce terminal scrollback, evict cold JIT
owners, and release inactive transfer buffers. Do not allocate a RAM snapshot.

## Metrics

Publish low-rate snapshots only while diagnostics are visible. Include:

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

- boot, serial input, serial output, and Worker destruction;
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
