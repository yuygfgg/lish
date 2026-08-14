import assert from "node:assert/strict";
import { sampleInstructionRate } from "../web/app/telemetry.mjs";

const first = sampleInstructionRate(null, 100n, 1_000);
assert.equal(first.instructionsPerSecond, 0);

const duplicate = sampleInstructionRate(first.next, 100n, 1_250);
assert.equal(duplicate.instructionsPerSecond, null);
assert.equal(duplicate.next.timestamp, 1_000);

const second = sampleInstructionRate(duplicate.next, 5_000_100n, 1_500);
assert.equal(second.instructionsPerSecond, 10_000_000);

const stale = sampleInstructionRate(second.next, 5_000_100n, 3_000);
assert.equal(stale.instructionsPerSecond, 0);
assert.equal(stale.next.timestamp, 1_500);

const reset = sampleInstructionRate(second.next, 0n, 2_000);
assert.equal(reset.instructionsPerSecond, 0);

const sameTimestamp = sampleInstructionRate(reset.next, 100n, 2_000);
assert.equal(sameTimestamp.instructionsPerSecond, 0);

console.log("native telemetry self-test: PASS");
