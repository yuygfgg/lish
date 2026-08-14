export type ImageSource =
  | Uint8Array
  | ArrayBuffer
  | ArrayBufferView
  | Response
  | { url: string };

export type NativeDiskSource = {
  /** Authenticated `/disk` endpoint. The client appends `offset` and `length`. */
  mode: "native";
  url: string;
  /** Raw image size. If omitted, the client obtains it with HEAD. */
  size?: number;
};

export type ExecutionConfig =
  | { mode: "local" }
  | {
      /** Run Wasm, devices, image downloads, and networking in a dedicated Worker. */
      mode: "worker";
      /** Module Worker entry point; defaults to rv64.worker.js beside rv64.js. */
      workerURL?: string | URL;
      /** Frequency of cached running/instruction state updates; defaults to 500ms. */
      statisticsIntervalMs?: number;
    };

export type BootConfig =
  | {
      mode: "linux-direct";
      kernel: ImageSource;
      initrd?: ImageSource;
      disk?: ImageSource | NativeDiskSource;
      cmdline?: string;
    }
  | {
      mode: "firmware";
      /** OpenSBI firmware image. */
      firmware: ImageSource;
      kernel?: ImageSource;
      initrd?: ImageSource;
      disk?: ImageSource | NativeDiskSource;
      cmdline?: string;
    };

export type NetworkConfig =
  | { mode: "none" }
  | {
      /** Raw Ethernet frames over a websockproxy-compatible WebSocket. */
      mode: "wsproxy";
      /** Layer-2 websockproxy-compatible relay URL. */
      url: string;
      protocols?: string | string[];
      mac?: Uint8Array;
    }
  | { mode: "external"; mac?: Uint8Array };

export interface DownloadProgress {
  image: "wasm" | "firmware" | "kernel" | "initrd" | "disk";
  loaded: number;
  total?: number;
}

export interface RV64EventMap {
  ready: undefined;
  start: undefined;
  stop: { reason: "requested" | "powered-off" | "error" };
  error: unknown;
  console: Uint8Array;
  networkTransmit: Uint8Array;
  downloadProgress: DownloadProgress;
}

export type RV64EventListeners = {
  [K in keyof RV64EventMap]?: (event: RV64EventMap[K]) => void;
};

export interface RV64Options {
  wasm: ImageSource;
  memoryMB?: number;
  /** Guest-code compilation settings and ownership bounds for this VM. */
  jit?: {
    /** Set to false to use only the interpreter. Defaults to true. */
    enabled?: boolean;
    maxModules?: number;
    maxSlots?: number;
    maxBytes?: number;
    growSlots?: number;
    /** Maximum concurrent background WebAssembly.compile jobs. Defaults to 4. */
    asyncCompilers?: number;
  };
  boot: BootConfig;
  /** Networking is disabled unless the caller selects a backend. */
  network?: NetworkConfig;
  /** Execution stays on the calling thread unless Worker mode is explicitly selected. */
  execution?: ExecutionConfig;
  events?: RV64EventListeners;
}

export interface JitMetrics {
  liveModules: number;
  liveSlots: number;
  liveBytes: number;
  pendingModules: number;
  pendingSlots: number;
  pendingBytes: number;
  rustPendingBuilds: number;
  pendingBlocks: number;
  pendingBatches: number;
  pendingRegions: number;
  asyncCompileActive: number;
  asyncCompileQueued: number;
  peakAsyncCompileQueued: number;
  [name: string]: number | boolean | object;
}

export class RV64 {
  static create(options: RV64Options): Promise<RV64>;
  readonly running: boolean;
  /** Exact in local mode; periodically sampled in Worker mode. */
  readonly instructions: bigint;
  /** JIT ownership and asynchronous compiler queue state. */
  jitMetrics(): JitMetrics | null;
  readonly console: { send(data: string | Uint8Array): void };
  readonly network: {
    readonly mode: NetworkConfig["mode"];
    /** Supply an Ethernet frame when network.mode is "external". */
    receive(frame: Uint8Array | ArrayBuffer): void;
  };
  on<K extends keyof RV64EventMap>(
    event: K,
    listener: (event: RV64EventMap[K]) => void,
  ): () => void;
  start(): Promise<void>;
  stop(): Promise<void>;
  reset(): Promise<void>;
  destroy(): Promise<void>;
}
