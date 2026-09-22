// zlib's one-shot decoders honour `maxOutputLength` -- gunzipSync, inflateSync,
// inflateRawSync, unzipSync and the callback forms -- and validate it as node
// does. An output past the cap is RangeError [ERR_BUFFER_TOO_LARGE] naming the
// caller's value; undefined and NaN mean no cap; a non-number is
// ERR_INVALID_ARG_TYPE; Infinity is "a finite number"; outside 1..kMaxLength is
// ERR_OUT_OF_RANGE; a fraction is accepted and compared as written.
//
// Regression guard: oam's one-shot functions read `level` and ignored every
// other option, so `gunzipSync(buf, { maxOutputLength: 1000 })` returned the
// whole inflated output. A 200 KB gzip of 200 MiB of spaces handed to
// fetch-mcp's sitemap tool with a 1 MiB cap inflated all 200 MiB and took the
// server down at oam's 4 GB heap cap. The cap has to hold while inflating,
// not on the finished buffer, so the last check inflates a 64 MiB bomb under
// a 64 KiB cap and prints only the error -- an implementation that inflates
// first and measures after still prints the same line, but takes the memory;
// the memory is held by the engine's own test, this case holds the contract.
//
// Only what both runtimes print byte for byte: error class, code and message,
// and output lengths. Compressed sizes are never printed.
import zlib from "node:zlib";

const describe = (e) => `${e.constructor.name} ${e.code} ${JSON.stringify(e.message)}`;
const attempt = (label, fn) => {
  try {
    const out = fn();
    console.log(`${label}: ok ${out.length}`);
  } catch (e) {
    console.log(`${label}: ${describe(e)}`);
  }
};

const text = Buffer.alloc(100000, 0x20);
const gz = zlib.gzipSync(text);
const zl = zlib.deflateSync(text);
const raw = zlib.deflateRawSync(text);

// The cap against the true size: below, exact, above, absent.
attempt("gunzip below", () => zlib.gunzipSync(gz, { maxOutputLength: 1000 }));
attempt("gunzip exact", () => zlib.gunzipSync(gz, { maxOutputLength: 100000 }));
attempt("gunzip above", () => zlib.gunzipSync(gz, { maxOutputLength: 100001 }));
attempt("gunzip none", () => zlib.gunzipSync(gz));
attempt("gunzip one", () => zlib.gunzipSync(gz, { maxOutputLength: 1 }));

// Every one-shot decoder, not only gunzip.
attempt("inflate", () => zlib.inflateSync(zl, { maxOutputLength: 4999 }));
attempt("inflateRaw", () => zlib.inflateRawSync(raw, { maxOutputLength: 10 }));
attempt("unzip gzip", () => zlib.unzipSync(gz, { maxOutputLength: 10 }));
attempt("unzip zlib", () => zlib.unzipSync(zl, { maxOutputLength: 10 }));

// Validation, node's checkRangesOrGetDefault.
for (const value of [0, -1, NaN, "x", Infinity, -Infinity, 1.5, 2 ** 53, null, true]) {
  attempt(`value ${String(value)}`, () => zlib.gunzipSync(gz, { maxOutputLength: value }));
}

// The encoders are held to it too.
attempt("gzip with cap", () => zlib.gzipSync(text, { maxOutputLength: 1 }));
attempt("gzip under cap", () => zlib.gzipSync(text, { maxOutputLength: 100000 }));

// Held while inflating: 64 MiB of spaces under a 64 KiB cap.
const bomb = zlib.gzipSync(Buffer.alloc(64 * 1024 * 1024, 0x20));
attempt("bomb", () => zlib.gunzipSync(bomb, { maxOutputLength: 65536 }));

// The callback forms: the same validation, thrown synchronously; the same
// cap, delivered to the callback.
const cb = (label) =>
  new Promise((resolve) => {
    zlib.gunzip(gz, { maxOutputLength: label === "async ok" ? 200000 : 10 }, (err, out) => {
      console.log(`${label}: ${err ? describe(err) : `ok ${out.length}`}`);
      resolve();
    });
  });
await cb("async cap");
await cb("async ok");
attempt("async bad value", () => zlib.gunzip(gz, { maxOutputLength: 0 }, () => {}));
