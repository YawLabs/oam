// zlib.createUnzip() tells gzip from zlib by the stream's first two bytes
// (the gzip magic 1f 8b). oam decided from the first WRITE alone, and a
// one-byte write can never start with both magic bytes -- so a gzip stream
// delivered `1f`, `8b`, `08`, ... one byte per write was decoded as zlib and
// failed with "corrupt deflate stream".
//
// That chunking is real: some servers flush the first bytes of a gzip body
// one at a time, and got runs every compressed response through
// decompress-response, which uses createUnzip() for `content-encoding: gzip`.
// A plain got() of such a response failed on oam with a misleading
// ECONNRESET.
//
// Every decoder x every format x write sizes of 1, 2, 3 bytes and one write.
// Only the outcome and the decoded length are printed: the error TEXT for a
// wrong-format stream is a separate concern.
import zlib from "node:zlib";

const plain = Buffer.from("x".repeat(5000) + "tail");
const inputs = {
  gzip: zlib.gzipSync(plain),
  zlib: zlib.deflateSync(plain),
  raw: zlib.deflateRawSync(plain),
};
const decoders = {
  createUnzip: () => zlib.createUnzip(),
  createGunzip: () => zlib.createGunzip(),
  createInflate: () => zlib.createInflate(),
  createInflateRaw: () => zlib.createInflateRaw(),
};

function run(make, input, size) {
  return new Promise((resolve) => {
    const stream = make();
    const out = [];
    let settled = false;
    const settle = (line) => {
      if (settled) return;
      settled = true;
      resolve(line);
    };
    stream.on("data", (chunk) => out.push(chunk));
    stream.on("error", () => settle("error"));
    stream.on("end", () => {
      const got = Buffer.concat(out);
      settle(got.equals(plain) ? `ok ${got.length}` : `wrong output (${got.length} bytes)`);
    });
    // One write per tick, so each slice really is its own write to the
    // decoder rather than being coalesced by the writable side.
    let offset = 0;
    const step = () => {
      if (settled) return;
      if (offset >= input.length) {
        stream.end();
        return;
      }
      stream.write(input.subarray(offset, offset + size));
      offset += size;
      setImmediate(step);
    };
    step();
  });
}

for (const [decoderName, make] of Object.entries(decoders)) {
  for (const [format, input] of Object.entries(inputs)) {
    const cells = [];
    for (const size of [1, 2, 3, input.length]) {
      const label = size === input.length ? "whole" : String(size);
      cells.push(`${label}: ${await run(make, input, size)}`);
    }
    console.log(`${decoderName.padEnd(16)} ${format.padEnd(4)} | ${cells.join(" | ")}`);
  }
}

// The first two bytes in separate writes, the rest in one: the exact shape
// of the failure (the decision has to span the two writes).
{
  const gz = inputs.gzip;
  const result = await new Promise((resolve) => {
    const stream = zlib.createUnzip();
    const out = [];
    stream.on("data", (c) => out.push(c));
    stream.on("error", () => resolve("error"));
    stream.on("end", () => resolve(`ok ${Buffer.concat(out).length}`));
    stream.write(gz.subarray(0, 1));
    setImmediate(() => {
      stream.write(gz.subarray(1, 2));
      setImmediate(() => stream.end(gz.subarray(2)));
    });
  });
  console.log("createUnzip gzip, 1 + 1 + rest:", result);
}

// A stream that ends on the lone first magic byte is a truncated gzip
// stream; the byte held for the decision must not be dropped into a clean
// empty result.
{
  const result = await new Promise((resolve) => {
    const stream = zlib.createUnzip();
    const out = [];
    stream.on("data", (c) => out.push(c));
    stream.on("error", () => resolve("error"));
    stream.on("end", () => resolve(`ok ${Buffer.concat(out).length}`));
    stream.end(Buffer.from([0x1f]));
  });
  console.log("createUnzip, one byte (1f) then end:", result);
}
