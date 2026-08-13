// Save an immutable Wasm candidate with enough source provenance to reproduce it.
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { constants } from "node:fs";
import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "../..");
const artifactsArg = process.env.ARTIFACTS;
if (!artifactsArg) {
  console.error("set ARTIFACTS=<artifacts directory>");
  process.exit(2);
}

const artifacts = resolve(artifactsArg);
const label = (process.argv[2] || "").replace(/[^a-zA-Z0-9._-]/g, "-");
if (!label) {
  console.error("usage: snapshot-wasm.mjs <label>");
  process.exit(2);
}

const source = process.env.WASM
  ? resolve(process.env.WASM)
  : join(root, "target/wasm32-unknown-unknown/release/rv64_wasm.wasm");
const bytes = await readFile(source);
const wasmSha = createHash("sha256").update(bytes).digest("hex");
const git = (...args) => {
  try {
    return execFileSync("git", args, { encoding: "utf8" }).trim();
  } catch {
    return "unknown";
  }
};

const diff = git("-C", root, "diff", "--binary", "HEAD");
const staged = git("-C", root, "diff", "--binary", "--cached", "HEAD");
const untracked = git("-C", root, "ls-files", "--others", "--exclude-standard");
const sourceState = createHash("sha256").update(diff).update("\0").update(staged).update("\0");
for (const path of untracked.split("\n").filter(Boolean).sort()) {
  sourceState.update(path).update("\0");
  try {
    sourceState.update(await readFile(join(root, path)));
  } catch {
    sourceState.update("<unreadable>");
  }
  sourceState.update("\0");
}

const outDir = join(artifacts, "wasm-candidates");
await mkdir(outDir, { recursive: true });
const stem = `${label}-${wasmSha.slice(0, 12)}`;
const wasmOut = join(outDir, `${stem}.wasm`);
const manifestOut = join(outDir, `${stem}.json`);
await copyFile(source, wasmOut, constants.COPYFILE_EXCL);
await writeFile(
  manifestOut,
  JSON.stringify({
    schema: 1,
    label,
    created: new Date().toISOString(),
    wasm: wasmOut,
    wasm_sha256: wasmSha,
    git: git("-C", root, "rev-parse", "HEAD"),
    git_status: git("-C", root, "status", "--short"),
    source_state_sha256: sourceState.digest("hex"),
    node: process.version,
  }, null, 2),
  { flag: "wx" },
);
console.log(`saved ${wasmOut}`);
console.log(`manifest ${manifestOut}`);
