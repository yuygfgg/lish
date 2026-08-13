#!/usr/bin/env node
import assert from "node:assert/strict";
import {
  NativeDiskClient,
  serviceNativeDiskRequest,
} from "../web/native-disk.js";

const RealFetch = globalThis.fetch;

try {
  await testSizeDiscoveryRequiresExplicitHeader();
  await testCacheLinesAndEviction();
  await testOrderedWritesFlushAndInvalidation();
  await testFailedWriteInvalidatesUncertainData();
  await testRangeValidationAndDestroy();
  await testGuestReceivesDiskErrorsWithoutStoppingTheHostLoop();
  await testDiagnosticsDescribeFailuresWithoutDumpingBodies();
} finally {
  globalThis.fetch = RealFetch;
  delete globalThis.LISH_DIAGNOSTICS;
}

console.log("PASS native disk client cache and ordering");

async function testSizeDiscoveryRequiresExplicitHeader() {
  globalThis.fetch = async (_url, options) => {
    assert.equal(options.method, "HEAD");
    assert.equal(options.cache, "no-store");
    return new Response(null, {
      status: 200,
      headers: {
        "content-length": "0",
        "x-lish-disk-size": "131072",
      },
    });
  };
  assert.equal(await NativeDiskClient.discoverSize("http://127.0.0.1/disk"), 131072);

  globalThis.fetch = async () => new Response(null, {
    status: 200,
    headers: { "content-length": "131072" },
  });
  await assert.rejects(
    NativeDiskClient.discoverSize("http://127.0.0.1/disk"),
    /did not return a safe image size/,
  );
}

async function testCacheLinesAndEviction() {
  const image = Uint8Array.from({ length: 32 }, (_, index) => index);
  const requests = [];
  globalThis.fetch = async (input) => {
    const url = new URL(input);
    const offset = Number(url.searchParams.get("offset"));
    const length = Number(url.searchParams.get("length"));
    requests.push([offset, length]);
    return new Response(image.slice(offset, offset + length));
  };
  const disk = await NativeDiskClient.create(
    { mode: "native", url: "http://127.0.0.1/disk", size: 512 },
    { cacheLineSize: 8, maximumCacheBytes: 16, maximumRequestBytes: 16 },
  );

  assert.deepEqual([...await disk.read(2, 3)], [2, 3, 4]);
  assert.deepEqual([...await disk.read(4, 8)], [4, 5, 6, 7, 8, 9, 10, 11]);
  assert.deepEqual(requests, [[0, 8], [8, 8]], "cached line was fetched more than once");

  await disk.read(16, 1);
  await disk.read(0, 1);
  assert.deepEqual(requests, [[0, 8], [8, 8], [16, 8], [0, 8]], "LRU eviction did not honor the byte limit");
}

async function testOrderedWritesFlushAndInvalidation() {
  const image = new Uint8Array(512);
  const order = [];
  globalThis.fetch = async (input, options = {}) => {
    const url = new URL(input);
    const method = options.method ?? "GET";
    if (method === "GET") {
      const offset = Number(url.searchParams.get("offset"));
      const length = Number(url.searchParams.get("length"));
      order.push(`read:${offset}`);
      return new Response(image.slice(offset, offset + length));
    }
    if (method === "PUT") {
      const offset = Number(url.searchParams.get("offset"));
      const body = new Uint8Array(await new Response(options.body).arrayBuffer());
      order.push(`write:${offset}`);
      image.set(body, offset);
      return new Response(null, { status: 204 });
    }
    assert.equal(method, "POST");
    order.push("flush");
    return new Response(null, { status: 204 });
  };
  const disk = await NativeDiskClient.create(
    { mode: "native", url: "http://127.0.0.1/disk", size: 512 },
    { cacheLineSize: 8, maximumCacheBytes: 16, maximumRequestBytes: 16 },
  );

  await disk.read(0, 1);
  const first = disk.write(0, Uint8Array.of(0xa1));
  const second = disk.write(8, Uint8Array.of(0xb2));
  const flush = disk.flush();
  await Promise.all([first, second, flush]);
  assert.deepEqual(order, ["read:0", "write:0", "write:8", "flush"]);

  order.length = 0;
  assert.deepEqual([...await disk.read(0, 1)], [0xa1]);
  assert.deepEqual([...await disk.read(16, 1)], [0]);
  await disk.write(0, Uint8Array.of(0xc3));
  assert.deepEqual([...await disk.read(16, 1)], [0], "non-overlapping cache line was evicted");
  assert.deepEqual([...await disk.read(0, 1)], [0xc3]);
  assert.deepEqual(order, ["read:0", "read:16", "write:0", "read:0"]);
}

async function testFailedWriteInvalidatesUncertainData() {
  const image = new Uint8Array(512);
  let reads = 0;
  globalThis.fetch = async (input, options = {}) => {
    const url = new URL(input);
    if ((options.method ?? "GET") === "GET") {
      reads++;
      const offset = Number(url.searchParams.get("offset"));
      const length = Number(url.searchParams.get("length"));
      return new Response(image.slice(offset, offset + length));
    }
    return new Response(null, { status: 500 });
  };
  const disk = await NativeDiskClient.create(
    { mode: "native", url: "http://127.0.0.1/disk", size: 512 },
    { cacheLineSize: 8, maximumCacheBytes: 8, maximumRequestBytes: 8 },
  );

  await disk.read(0, 1);
  await assert.rejects(disk.write(0, Uint8Array.of(1)), /disk write failed: 500/);
  await disk.read(0, 1);
  assert.equal(reads, 2, "failed write left potentially stale clean data cached");
}

async function testRangeValidationAndDestroy() {
  globalThis.fetch = async () => new Response(new Uint8Array(8));
  const disk = await NativeDiskClient.create(
    { mode: "native", url: "http://127.0.0.1/disk", size: 512 },
    { cacheLineSize: 8, maximumCacheBytes: 8, maximumRequestBytes: 8 },
  );
  assert.throws(() => disk.read(0, 9), /outside the image/);
  assert.throws(() => disk.write(512, Uint8Array.of(1)), /outside the image/);
  assert.throws(
    () => new NativeDiskClient("http://127.0.0.1/disk?offset=1", 512),
    /must not contain a query or fragment/,
  );
  disk.destroy();
  await assert.rejects(disk.read(0, 1), /destroyed/);
}

async function testGuestReceivesDiskErrorsWithoutStoppingTheHostLoop() {
  const completions = [];
  const core = {
    virtDiskComplete(data, ok) {
      completions.push({ data, ok });
      return true;
    },
  };
  const disk = {
    read: async () => { throw new Error("host read failed"); },
    write: async () => { throw new Error("host write failed"); },
    flush: async () => { throw new Error("host flush failed"); },
  };

  await serviceNativeDiskRequest(core, disk, {
    id: 1n,
    kind: "read",
    offset: 0n,
    length: 1n,
  });
  await serviceNativeDiskRequest(core, disk, {
    id: 2n,
    kind: "write",
    offset: 0n,
    length: 1n,
    body: Uint8Array.of(1),
  });
  await serviceNativeDiskRequest(core, disk, {
    id: 3n,
    kind: "flush",
    offset: 0n,
    length: 0n,
  });
  assert.deepEqual(completions, [
    { data: undefined, ok: false },
    { data: undefined, ok: false },
    { data: undefined, ok: false },
  ]);

  await assert.rejects(
    serviceNativeDiskRequest(core, disk, { id: 4n, kind: "unknown" }),
    /unknown native disk request/,
  );
  await assert.rejects(
    serviceNativeDiskRequest(
      { virtDiskComplete: () => false },
      { flush: async () => {} },
      { id: 5n, kind: "flush" },
    ),
    /rejected native disk completion/,
  );
}

async function testDiagnosticsDescribeFailuresWithoutDumpingBodies() {
  const realError = console.error;
  const messages = [];
  globalThis.LISH_DIAGNOSTICS = true;
  console.error = (...values) => messages.push(values);
  try {
    await serviceNativeDiskRequest(
      { virtDiskComplete: (_data, ok) => !ok },
      { write: async () => { throw new Error("transport failed"); } },
      {
        id: 6n,
        kind: "write",
        offset: 4096n,
        length: 512n,
        body: Uint8Array.of(0xde, 0xad, 0xbe, 0xef),
      },
    );
  } finally {
    console.error = realError;
    delete globalThis.LISH_DIAGNOSTICS;
  }
  assert.deepEqual(messages, [[
    "native disk request failed",
    {
      kind: "write",
      offset: "4096",
      length: "512",
      error: "transport failed",
    },
  ]]);
  assert.doesNotMatch(JSON.stringify(messages), /deadbeef/i);
}
