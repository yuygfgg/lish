export function sampleInstructionRate(previous, instructions, now) {
  const current = BigInt(instructions);
  const timestamp = Number(now);
  const next = { instructions: current, timestamp };
  if (!previous || !Number.isFinite(timestamp) || timestamp <= previous.timestamp) {
    return { next, instructionsPerSecond: 0 };
  }
  const delta = current >= previous.instructions
    ? current - previous.instructions
    : 0n;
  const rate = Number(delta) * 1000 / (timestamp - previous.timestamp);
  return { next, instructionsPerSecond: Number.isFinite(rate) ? rate : 0 };
}
