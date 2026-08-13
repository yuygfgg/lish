const OP_IMM = 0x13;
const LUI = 0x37;
const SBI_SRST = 0x5352_5354;

function encodeAddImmediate(rd, rs1, immediate) {
  return (
    OP_IMM |
    (rd << 7) |
    (rs1 << 15) |
    ((immediate & 0xfff) << 20)
  ) >>> 0;
}

function encodeLoadUpperImmediate(rd, immediate) {
  return (LUI | (rd << 7) | ((immediate & 0xfffff) << 12)) >>> 0;
}

export function idleKernel() {
  const image = new Uint8Array(4096);
  new DataView(image.buffer).setUint32(0, 0x1050_0073, true);
  return image;
}

export function powerOffKernel() {
  const upper = Math.floor((SBI_SRST + 0x800) / 0x1000);
  const lower = SBI_SRST - upper * 0x1000;
  const words = [
    encodeLoadUpperImmediate(17, upper),
    encodeAddImmediate(17, 17, lower),
    encodeAddImmediate(16, 0, 0),
    encodeAddImmediate(10, 0, 0),
    0x0000_0073,
  ];
  const image = new Uint8Array(4096);
  const view = new DataView(image.buffer);
  words.forEach((word, index) => view.setUint32(index * 4, word, true));
  return image;
}
