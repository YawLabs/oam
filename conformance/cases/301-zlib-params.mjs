// A zlib stream's params(level, strategy, callback): node validates both as
// "level" and "strategy" (-1..9 and 0..4, the same checks as the options),
// calls back on the next tick when both match what the stream has (a missing
// callback is then ERR_INVALID_ARG_TYPE), and otherwise flushes the stream
// with Z_SYNC_FLUSH -- after the writes before it -- and changes the level
// for what follows. Its binding changes the level of a deflate or deflateRaw
// stream only: a gzip stream flushes and keeps its level. It reads the
// level as an int32, so params(undefined, undefined) stores the rest.
//
// Regression guard: oam's zlib streams had no params(), so
// `deflate.params(9, 0, cb)` was a TypeError, and its deflaters (flate2's
// encoders) could not change level once started.
//
// Prints validation errors, callback order, round trips, and only coarse
// sizes (stored or compressed): the compressed bytes are miniz's, not zlib's.
import zlib from "node:zlib";

const describe = (e) => `${e.constructor.name} ${e.code} ${JSON.stringify(e.message)}`;
const attempt = (label, fn) => {
  try {
    console.log(`${label}: ok ${fn()}`);
  } catch (e) {
    console.log(`${label}: ${describe(e)}`);
  }
};

const show = (v) => (typeof v === "string" ? JSON.stringify(v) : String(v));
for (const name of ["Deflate", "Inflate", "Gzip", "Unzip", "BrotliCompress"]) {
  const stream = new zlib[name]();
  for (const [level, strategy] of [[99, 0], [-2, 0], [1, 5], [1, -1], [null, 0], [1, null], ["1", 0], [Infinity, 0], [1, 4.5]]) {
    attempt(`${name}.params(${show(level)}, ${show(strategy)})`, () => stream.params(level, strategy, () => {}));
  }
  stream.destroy();
}
{
  const stream = new zlib.Deflate();
  attempt("same, no callback", () => stream.params(-1, 0));
  attempt("same, callback 5", () => stream.params(-1, 0, 5));
  stream.destroy();
}

const text = "hello hello hello hello world ".repeat(200);
const inflaters = { Deflate: zlib.inflateSync, DeflateRaw: zlib.inflateRawSync, Gzip: zlib.gunzipSync };
const run = (name, level, setup) => new Promise((resolve) => {
  const stream = new zlib[name]({ level });
  const out = [];
  stream.on("data", (c) => out.push(c));
  stream.on("end", () => {
    const buf = Buffer.concat(out);
    const back = inflaters[name](buf).toString();
    const size = buf.length > 2 * text.length ? "all stored" : buf.length > text.length ? "half stored" : "compressed";
    console.log(`${name} ${level}: round trip ${back === text + text}, ${size}`);
    resolve();
  });
  setup(stream);
});

// The level changes between the two writes (deflate, deflateRaw), or not (gzip).
for (const name of ["Deflate", "DeflateRaw", "Gzip"]) {
  for (const [from, to] of [[9, 0], [0, 9], [-1, 0]]) {
    await run(name, from, (stream) => {
      stream.write(text);
      stream.params(to, 0, () => {
        console.log(`${name} ${from} -> ${to}: called back, level ${stream._level}`);
        stream.end(text);
      });
    });
  }
}

// Same values: next tick, no flush.
await run("Deflate", 1, (stream) => {
  stream.write(text);
  let sync = true;
  stream.params(1, 0, (...args) => {
    console.log(`same: ${args.length} args, async ${!sync}`);
    stream.end(text);
  });
  sync = false;
});

// Before any write, and with undefined (level 0).
await run("Deflate", 9, (stream) => {
  stream.params(undefined, undefined, () => {
    console.log(`undefined: level ${stream._level}, strategy ${stream._strategy}`);
    stream.write(text);
    stream.end(text);
  });
});

// The flush follows the writes queued before it: the first write's data is
// out by the time the callback runs.
await new Promise((resolve) => {
  const stream = new zlib.Deflate({ level: 6 });
  const out = [];
  stream.on("data", (c) => out.push(c));
  stream.write(text);
  stream.params(2, 0, () => {
    const sofar = Buffer.concat(out);
    const head = zlib.inflateSync(sofar, { finishFlush: zlib.constants.Z_SYNC_FLUSH }).toString();
    console.log(`flushed before callback: ${head === text}`);
    stream.end();
    stream.resume();
    stream.on("end", resolve);
  });
});
