const DEFAULT_CACHE_LINE_SIZE = 64 * 1024;
const DEFAULT_CACHE_MAX_BYTES = 64 * 1024 * 1024;
const DEFAULT_MAXIMUM_REQUEST_BYTES = 64 * 1024;

export function isNativeDisk(value) {
  return value &&
    typeof value === "object" &&
    value.mode === "native" &&
    typeof value.url === "string";
}

export async function serviceNativeDiskRequest(core, disk, request) {
  let data;
  let ok = false;
  switch (request.kind) {
    case "read":
      try {
        data = await disk.read(Number(request.offset), Number(request.length));
        ok = true;
      } catch {}
      break;
    case "write":
      try {
        await disk.write(Number(request.offset), request.body);
        ok = true;
      } catch {}
      break;
    case "flush":
      try {
        await disk.flush();
        ok = true;
      } catch {}
      break;
    default:
      throw new Error(`unknown native disk request: ${request.kind}`);
  }
  if (!core.virtDiskComplete(data, ok)) {
    throw new Error(`VM rejected native disk completion for request ${request.id}`);
  }
}

export class NativeDiskClient {
  #url;
  #size;
  #cache = new Map();
  #tail = Promise.resolve();
  #cacheLineSize;
  #maximumCacheLines;
  #maximumRequestBytes;
  #closed = false;

  constructor(url, size, options = {}) {
    this.#url = normalizeDiskURL(url);
    this.#size = size;
    this.#cacheLineSize = positiveInteger(
      options.cacheLineSize ?? DEFAULT_CACHE_LINE_SIZE,
      "disk cache line size",
    );
    const maximumCacheBytes = positiveInteger(
      options.maximumCacheBytes ?? DEFAULT_CACHE_MAX_BYTES,
      "disk cache size",
    );
    if (maximumCacheBytes < this.#cacheLineSize) {
      throw new RangeError("disk cache must hold at least one cache line");
    }
    this.#maximumCacheLines = Math.floor(maximumCacheBytes / this.#cacheLineSize);
    this.#maximumRequestBytes = positiveInteger(
      options.maximumRequestBytes ?? DEFAULT_MAXIMUM_REQUEST_BYTES,
      "maximum disk request size",
    );
  }

  static async create(config, options) {
    const url = normalizeDiskURL(config.url);
    const size = config.size === undefined
      ? await NativeDiskClient.discoverSize(url)
      : config.size;
    if (!Number.isSafeInteger(size) || size <= 0 || size % 512 !== 0) {
      throw new Error("native disk size must be a positive multiple of 512 bytes");
    }
    return new NativeDiskClient(url, size, options);
  }

  static async discoverSize(url) {
    const response = await fetch(normalizeDiskURL(url), {
      method: "HEAD",
      cache: "no-store",
    });
    if (!response.ok) throw new Error(`disk HEAD failed: ${response.status}`);
    const header = response.headers.get("x-lish-disk-size");
    const value = header === null || header.trim() === "" ? Number.NaN : Number(header);
    if (!Number.isSafeInteger(value) || value <= 0) {
      throw new Error("disk service did not return a safe image size");
    }
    return value;
  }

  get size() {
    return this.#size;
  }

  read(offset, length) {
    this.#validateRange(offset, length);
    return this.#enqueue(async () => {
      const result = new Uint8Array(length);
      let copied = 0;
      while (copied < length) {
        const position = offset + copied;
        const lineOffset = Math.floor(position / this.#cacheLineSize) * this.#cacheLineSize;
        const line = await this.#line(lineOffset);
        const start = position - lineOffset;
        const count = Math.min(length - copied, line.length - start);
        result.set(line.subarray(start, start + count), copied);
        copied += count;
      }
      return result;
    });
  }

  write(offset, bytes) {
    const body = bytes instanceof Uint8Array ? bytes.slice() : new Uint8Array(bytes).slice();
    this.#validateRange(offset, body.length);
    return this.#enqueue(async () => {
      const url = new URL(this.#url);
      url.searchParams.set("offset", String(offset));
      try {
        const response = await fetch(url, {
          method: "PUT",
          body,
          cache: "no-store",
          headers: { "content-type": "application/octet-stream" },
        });
        if (!response.ok) throw new Error(`disk write failed: ${response.status}`);
      } finally {
        // A transport failure can occur after the host committed the write.
        // Eviction keeps the clean cache correct when the outcome is uncertain.
        this.#invalidate(offset, body.length);
      }
    });
  }

  flush() {
    return this.#enqueue(async () => {
      const url = new URL(this.#url);
      url.pathname = `${url.pathname.replace(/\/$/, "")}/flush`;
      const response = await fetch(url, { method: "POST", cache: "no-store" });
      if (!response.ok) throw new Error(`disk flush failed: ${response.status}`);
    });
  }

  destroy() {
    this.#closed = true;
    this.#cache.clear();
  }

  #line(offset) {
    const cached = this.#cache.get(offset);
    if (cached) {
      this.#cache.delete(offset);
      this.#cache.set(offset, cached);
      return cached;
    }
    const url = new URL(this.#url);
    const expected = Math.min(this.#cacheLineSize, this.#size - offset);
    url.searchParams.set("offset", String(offset));
    url.searchParams.set("length", String(expected));
    return fetch(url, { cache: "no-store" }).then(async (response) => {
      if (!response.ok) throw new Error(`disk read failed: ${response.status}`);
      const bytes = new Uint8Array(await response.arrayBuffer());
      if (bytes.length !== expected) {
        throw new Error("disk service returned an invalid range");
      }
      this.#cache.set(offset, bytes);
      while (this.#cache.size > this.#maximumCacheLines) {
        this.#cache.delete(this.#cache.keys().next().value);
      }
      return bytes;
    });
  }

  #invalidate(offset, length) {
    const end = offset + length;
    for (const lineOffset of this.#cache.keys()) {
      if (lineOffset < end && lineOffset + this.#cacheLineSize > offset) {
        this.#cache.delete(lineOffset);
      }
    }
  }

  #enqueue(operation) {
    if (this.#closed) return Promise.reject(new Error("native disk client is destroyed"));
    const result = this.#tail.then(operation, operation);
    this.#tail = result.then(() => undefined, () => undefined);
    return result;
  }

  #validateRange(offset, length) {
    if (
      !Number.isSafeInteger(offset) ||
      !Number.isSafeInteger(length) ||
      length <= 0 ||
      length > this.#maximumRequestBytes ||
      offset < 0 ||
      offset > this.#size - length
    ) {
      throw new RangeError("disk range is outside the image");
    }
  }
}

function normalizeDiskURL(value) {
  const url = new URL(value, globalThis.location?.href ?? import.meta.url);
  if (url.search || url.hash) {
    throw new TypeError("native disk URL must not contain a query or fragment");
  }
  return url;
}

function positiveInteger(value, name) {
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new RangeError(`${name} must be a positive safe integer`);
  }
  return value;
}
