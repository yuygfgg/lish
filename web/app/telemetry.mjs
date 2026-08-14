const STALE_SAMPLE_MS = 1_500;

export function sampleInstructionRate(previous, instructions, now) {
  const current = BigInt(instructions);
  const timestamp = Number(now);
  const next = { instructions: current, timestamp };
  if (!previous || !Number.isFinite(timestamp) || timestamp <= previous.timestamp) {
    return { next, instructionsPerSecond: 0 };
  }
  if (current === previous.instructions) {
    const stale = timestamp - previous.timestamp >= STALE_SAMPLE_MS;
    return {
      next: previous,
      instructionsPerSecond: stale ? 0 : null,
    };
  }
  const delta = current >= previous.instructions
    ? current - previous.instructions
    : 0n;
  const rate = Number(delta) * 1000 / (timestamp - previous.timestamp);
  return { next, instructionsPerSecond: Number.isFinite(rate) ? rate : 0 };
}
