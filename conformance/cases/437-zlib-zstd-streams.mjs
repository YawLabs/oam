// node:zlib's zstd streams (node v22.22.2): createZstdCompress /
// createZstdDecompress as node's ZlibBase drives them. A write is compressed
// under ZSTD_e_continue; end() then runs ZSTD_e_end, and a last chunk that
// was still queued when end() came is itself written under ZSTD_e_end -- so
// that stream ends with a second, empty frame. flush() writes under
// ZSTD_e_flush (or the directive given; a zlib value past ZSTD_e_end is
// libzstd's parameter_outOfBound). A decompress stream decodes a frame
// however the writes split it and a frame per write, ends its readable side
// when a frame ends with input left in the same write (or errors with
// ERR_TRAILING_JUNK_AFTER_STREAM_END under rejectGarbageAfterEnd), errors
// with node's coded zstd error on bad input, and ends without error on a
// frame cut short. reset() starts a new frame with a fresh context, which in
// node keeps the pledged size but not the parameters. A wrong pledged size
// fails the frame. The output arrives in chunkSize windows.
//
// Regression guard: oam had no zstd streams.
//
// Every scenario runs to 'close' before the next starts, so the output is
// ordered by events alone.
import zlib from "node:zlib";
import { pipeline } from "node:stream/promises";
import { Readable, Writable } from "node:stream";

const C = zlib.constants;
const hex = (b) => Buffer.from(b).toString("hex");
const frameA = Buffer.from("28b52ffd200109000061", "hex");
const frameB = Buffer.from("28b52ffd200109000062", "hex");

function run(label, make, drive) {
  return new Promise((resolve) => {
    let stream;
    try {
      stream = make();
    } catch (e) {
      console.log(`${label}: construct ${e.constructor.name} ${e.code} ${JSON.stringify(e.message)}`);
      resolve();
      return;
    }
    const chunks = [];
    stream.on("data", (d) => chunks.push(hex(d)));
    stream.on("error", (e) => {
      console.log(
        `${label}: error ${e.constructor.name} code=${e.code} errno=${e.errno} ` +
        `${JSON.stringify(e.message)} data=[${chunks.join("|")}]`,
      );
    });
    stream.on("end", () => {
      console.log(`${label}: end data=[${chunks.join("|")}] bytesWritten=${stream.bytesWritten}`);
    });
    stream.on("close", () => {
      console.log(`${label}: close closed=${stream._closed}`);
      resolve();
    });
    drive(stream);
  });
}

await run("compress, two writes then end()", () => zlib.createZstdCompress(), (s) => {
  s.write("hello ");
  s.write("world");
  s.end();
});
await run("compress, end(chunk) only", () => zlib.createZstdCompress(), (s) => s.end("hello"));
await run("compress, nothing", () => zlib.createZstdCompress(), (s) => s.end());
await run("compress, write after the first finished", () => zlib.createZstdCompress(), (s) => {
  s.write("hello ", () => s.end("world"));
});
await run("compress, flush()", () => zlib.createZstdCompress(), (s) => {
  s.write("hello");
  s.flush(() => {
    console.log("  flushed");
    s.end("x");
  });
});
await run("compress, flush(ZSTD_e_end)", () => zlib.createZstdCompress(), (s) => {
  s.write("hello");
  s.flush(C.ZSTD_e_end, () => s.end("x"));
});
await run("compress, flush(Z_FINISH)", () => zlib.createZstdCompress(), (s) => {
  s.write("hello");
  s.flush(C.Z_FINISH, () => console.log("  flush callback"));
});
await run("compress, flush after end", () => zlib.createZstdCompress(), (s) => {
  s.end("abc");
  s.flush(() => console.log("  flush after end called back"));
});
await run("compress, pledgedSrcSize right", () => zlib.createZstdCompress({ pledgedSrcSize: 5 }), (s) => {
  s.write("hel");
  s.end("lo");
});
await run("compress, pledgedSrcSize wrong", () => zlib.createZstdCompress({ pledgedSrcSize: 3 }), (s) => {
  s.write("hel");
  s.end("lo");
});
await run("compress, checksum then reset()", () => zlib.createZstdCompress({ params: { [C.ZSTD_c_checksumFlag]: 1 } }), (s) => {
  s.write("abc");
  s.flush(() => {
    s.reset();
    s.end("def");
  });
});
await run("compress, dictionary", () => zlib.createZstdCompress({ dictionary: Buffer.from("hello world hello world") }), (s) => s.end("hello world"));
await run("compress, flush option ZSTD_e_flush", () => zlib.createZstdCompress({ flush: C.ZSTD_e_flush }), (s) => {
  s.write("ab");
  s.write("cd");
  s.end();
});
await run("compress, chunkSize 64", () => zlib.createZstdCompress({ chunkSize: 64 }), (s) => {
  const b = Buffer.alloc(3000);
  for (let i = 0; i < b.length; i++) b[i] = ((i * 7919) >> 3) & 255;
  s.end(b);
});
await run("compress, close() mid-stream", () => zlib.createZstdCompress(), (s) => {
  s.write("hello");
  s.close(() => console.log("  close callback"));
});

await run("decompress, two frames in one write", () => zlib.createZstdDecompress(), (s) => s.end(Buffer.concat([frameA, frameB])));
await run("decompress, a frame per write", () => zlib.createZstdDecompress(), (s) => {
  s.write(frameA);
  s.end(frameB);
});
await run("decompress, second frame after the first finished", () => zlib.createZstdDecompress(), (s) => {
  s.write(frameA, () => s.end(frameB));
});
await run("decompress, junk after a finished frame", () => zlib.createZstdDecompress(), (s) => {
  s.write(frameA, () => s.end(Buffer.from("zz")));
});
await run("decompress, junk in the frame's write", () => zlib.createZstdDecompress(), (s) => s.end(Buffer.concat([frameA, Buffer.from("zz")])));
await run("decompress, rejectGarbageAfterEnd", () => zlib.createZstdDecompress({ rejectGarbageAfterEnd: true }), (s) => s.end(Buffer.concat([frameA, Buffer.from("zz")])));
await run("decompress, garbage", () => zlib.createZstdDecompress(), (s) => s.end(Buffer.from("garbage!")));
await run("decompress, cut short", () => zlib.createZstdDecompress(), (s) => s.end(frameA.subarray(0, frameA.length - 1)));
await run("decompress, a byte per write", () => zlib.createZstdDecompress(), (s) => {
  for (const byte of zlib.zstdCompressSync("hello hello hello")) s.write(Buffer.from([byte]));
  s.end();
});
await run("decompress, dictionary", () => zlib.createZstdDecompress({ dictionary: Buffer.from("hello world hello world") }), (s) => {
  s.end(zlib.zstdCompressSync("hello world", { dictionary: Buffer.from("hello world hello world") }));
});
await run("decompress, dictionary missing", () => zlib.createZstdDecompress(), (s) => {
  s.end(zlib.zstdCompressSync("hello world", { dictionary: Buffer.from("hello world hello world") }));
});
await run("decompress, maxOutputLength not applied to streams", () => zlib.createZstdDecompress({ maxOutputLength: 3 }), (s) => {
  s.end(zlib.zstdCompressSync("hello hello hello"));
});
await run("decompress, chunkSize 64", () => zlib.createZstdDecompress({ chunkSize: 64 }), (s) => {
  s.end(zlib.zstdCompressSync(Buffer.alloc(200, "x")));
});
await run("decompress, bad windowLogMax", () => zlib.createZstdDecompress({ params: { [C.ZSTD_d_windowLogMax]: 5 } }), (s) => s.end());

// _processChunk without a callback: the sync path, which closes the handle.
{
  const s = zlib.createZstdCompress();
  const out = s._processChunk(Buffer.from("hey"), C.ZSTD_e_end);
  console.log("_processChunk sync:", hex(out), "closed:", s._closed, "bytesWritten:", s.bytesWritten);
}

// A pipeline round trip of a large input with a slow reader (backpressure).
{
  const input = Buffer.alloc(1 << 20);
  for (let i = 0; i < input.length; i++) input[i] = ((i * 2654435761) >>> 24) & 0x3f;
  const pieces = [];
  for (let i = 0; i < input.length; i += 10000) pieces.push(input.subarray(i, i + 10000));
  const packed = [];
  await pipeline(
    Readable.from(pieces),
    zlib.createZstdCompress({ params: { [C.ZSTD_c_compressionLevel]: 5 } }),
    new Writable({ write(chunk, _e, cb) { packed.push(chunk); cb(); } }),
  );
  // Not its bytes: whether the last piece is still queued when the source
  // ends (adding an empty frame) is a race between the two sides.
  const whole = Buffer.concat(packed);
  const out = [];
  await pipeline(
    Readable.from([whole.subarray(0, 1000), whole.subarray(1000)]),
    zlib.createZstdDecompress(),
    new Writable({
      highWaterMark: 1,
      write(chunk, _e, cb) { out.push(chunk); setImmediate(cb); },
    }),
  );
  const back = Buffer.concat(out);
  console.log("pipeline decompress:", back.length, back.equals(input), "chunks:", out.length);
}
