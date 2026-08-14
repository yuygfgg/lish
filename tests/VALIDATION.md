# Validation

Lish has one active machine contract: a RISC-V `virt` full-system machine
booted through the direct Linux SBI path. The browser runtime uses the same
machine and the same asynchronous WebAssembly JIT as the native diagnostic
runner.

## Required checks

Run the maintained local checks from the repository root:

```sh
tools/cargo fmt --all -- --check
tools/cargo clippy --workspace --all-targets -- -D warnings
tools/cargo test --workspace --exclude rv64-wasm --release
tools/cargo test -p rv64-wasm --lib --release
tools/cargo build -p rv64-wasm --target wasm32-unknown-unknown --release
node tests/boot-profile-selftest.mjs
node tests/jit-code-store.mjs
node tests/host-callback-boundary.mjs
node tests/public-api.mjs
node tests/virt-jit.mjs
node tests/worker-api.mjs
swift test --package-path native -Xswiftc -warnings-as-errors
```

`tests/run-all.sh` runs the same product checks and adds optional ISA, Spike,
architecture-signature, Alpine boot, and native virt smoke stages. Set
`REQUIRE_ALL=1` when every external tool and image must be present.

## CPU and JIT coverage

- `tests/run-isa-tests.sh` builds and runs the official `riscv-tests` ISA
  programs with `rv64-isa-test`.
- `tests/run-arch-tests.sh` compares architecture-test signatures with Spike.
- `tests/lockstep.py` compares committed integer register writes with Spike.
- `tests/virt-jit.mjs` covers full-system dispatch, SBI shutdown, TLB refill,
  host output boundaries, WFI, and asynchronous compilation invalidation.
- `tests/host-callback-boundary.mjs` covers the current host import ABI,
  deferred output delivery, and asynchronous JIT ownership failures.
- `tests/jit-code-store.mjs` covers bounded module ownership, slot reuse,
  reservation rollback, generation invalidation, and destruction.

The CPU tests allow the machine contract to execute misaligned accesses in
hardware. Spike configurations that trap such accesses are not equivalent
machine configurations and must not be treated as Lish failures.

## Browser and native integration

`tests/worker-api.mjs` checks the dedicated Worker lifecycle and statistics
boundary. `tests/alpine-boot.mjs` boots the current Alpine image in the direct
Linux machine and requires the `ALPINE_READY` marker. The test fails when the
Wasm module, kernel, or disk is absent; prepare them with
`web/prepare-images.sh`.

The Swift tests cover DHCP, ARP, UDP, libslirp forwarding, and the raw Ethernet
WebSocket framing used by the browser network mode. The browser network path
must carry Ethernet frames. HTTP, TLS, WISP, and 9P translation tests are not
part of the Lish contract.

Use `tests/https-benchmark.html` to measure cold TLS startup through the raw
Ethernet path. Build the network host:

```sh
swift build -c release --product lish-network-host --package-path native
```

Start the asset server in one terminal:

```sh
python3 -m http.server 4173
```

Start the network host in another terminal:

```sh
native/.build/release/lish-network-host \
  --origin http://127.0.0.1:4173 \
  --port 4199 \
  --capability 0123456789abcdef0123456789abcdef
```

Open this URL in WebKit:

```text
http://127.0.0.1:4173/tests/https-benchmark.html?execution=worker&trials=3&network=ws%3A%2F%2F127.0.0.1%3A4199%2F&capability=0123456789abcdef0123456789abcdef
```

Set `asyncCompilers=1..4` to compare compiler concurrency. Set `jit=off` for
an interpreter-only run. Set `target` to test another HTTPS URL. The page
records TCP SYN, ClientHello, peer close, command results, and JIT ownership
metrics in `globalThis.__lishHttpsBenchmark`.

## Performance and memory records

Performance results are development observations, not compatibility promises.
Record the browser engine, OS version, device, Wasm image hash, guest RAM,
JIT limits, workload, elapsed time, retired instructions, and process memory.
Record JIT live and pending module metrics with each long-running WebKit test.

Use WebKit as the default browser integration engine. Use Chromium only as a
comparison engine. A one-hour idle run must show bounded JIT ownership and a
stable memory band before a result is accepted.
