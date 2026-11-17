// node:_http_common's HTTPParser held to node v22.23.5's binding over llhttp
// 9.4.4 (case 366 covers the callback protocol): an empty value is judged on
// the byte after its line (a continuation line is a bad value byte, or with
// lenient headers goes on with the value; an empty Content-Length or -- new
// in llhttp 9.4.3 -- Transfer-Encoding is refused there, so a bare LF or a
// split buffer moves the offset); chunk extensions follow their grammar
// (token names, token or quoted-string values, quoted-pairs) and node's 16 KiB
// limit per chunk; a request's trailers count against maxHeaderPairs afresh
// and the head byte limit restarts for them; trailers set llhttp's flags
// (Connection: close, a response's non-chunked Transfer-Encoding);
// node's have_flushed_ is sticky, so after a head of 32+ fields (or any
// trailers) every later head goes to kOnHeaders; a url, status or field
// name passing the head limit at the end of a buffer overflows there, but an
// invalid byte is reported as such; HTTP/0.9 request lines; a space after
// the start line; obs-folds keep their whitespace and a folded
// Transfer-Encoding is not chunked; a non-chunked Transfer-Encoding beats
// Content-Length; 1xx other than 100-103 have bodies; Content-Length,
// Transfer-Encoding and Connection values are read a byte at a time (split
// buffers, OWS, obs-folds), and so are the start line's method, url and
// protocol; kOnHeadersComplete's answer is read as node reads it; finish()
// past a pause. oam's port skipped
// every extension byte to the CR, never counted trailers, judged empty
// values at the CR, and read the rest as llhttp 9.3 did or not at all.
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const { HTTPParser } = require("_http_common");

const show = (v) => JSON.stringify(v, (k, x) => (x === undefined ? "U" : x));

function run(type, chunks, opts = {}) {
  const out = [];
  const p = new HTTPParser();
  p.initialize(type, {}, opts.max || 0, opts.lenient || 0);
  if (opts.pairs !== undefined) p.maxHeaderPairs = opts.pairs;
  p[HTTPParser.kOnHeaders] = (h, u) => out.push("H" + show([h, u]));
  if (!opts.noHC) {
    p[HTTPParser.kOnHeadersComplete] = (ma, mi, h, m, u, s, r, up, ka) => {
      out.push("HC" + show([ma + "." + mi, h, u, s, up, ka]));
      return opts.hcRet;
    };
  }
  p[HTTPParser.kOnBody] = (b) => out.push("B" + show(b.toString("latin1")));
  p[HTTPParser.kOnMessageComplete] = () => out.push("MC");
  if (opts.bytes) chunks = [...chunks.join("")];
  const rets = [];
  for (const c of chunks) {
    const r = p.execute(Buffer.from(c, "latin1"));
    if (r instanceof Error) {
      out.push("ERR " + show([r.code, r.reason, r.bytesParsed]));
      break;
    }
    if (opts.bytes) rets.push(r);
    else out.push("ret " + r);
  }
  if (opts.bytes) out.push("rets " + rets.join(""));
  for (let k = 0; k < (opts.finish || 0); k++) {
    const f = p.finish();
    out.push("finish " + (f instanceof Error ? show([f.code, f.reason]) : show(f)));
  }
  return out.join(" | ");
}

const R = HTTPParser.REQUEST;
const S = HTTPParser.RESPONSE;
const RQ = "POST / HTTP/1.1\r\n";
const RS = "HTTP/1.1 200 OK\r\n";
const CH = "Transfer-Encoding: chunked\r\n";
const ext = (e) => CH + "\r\n1" + e + "\r\nx\r\n0\r\n\r\n";
const LH = HTTPParser.kLenientHeaders;
const LCR = HTTPParser.kLenientOptionalCRBeforeLF;
const cases = [];
for (const [t, type, line] of [["req", R, RQ], ["res", S, RS]]) {
  // Empty values.
  for (const h of ["Transfer-Encoding", "Content-Length", "X"]) {
    cases.push([`${t} empty ${h} then field`, type, [line + h + ":\r\nY: 1\r\n\r\n"]]);
    cases.push([`${t} empty ${h} then end`, type, [line + h + ": \t\r\n\r\n"]]);
    cases.push([`${t} empty ${h} split at LF`, type, [line + h + ":\r\n", "Y: 1\r\n\r\n"]]);
    cases.push([`${t} empty ${h} bare LF`, type, [line + h + ":\nY: 1\r\n\r\n"]]);
    cases.push([`${t} empty ${h} bare LF lenient`, type, [line + h + ":\nY: 1\r\n\r\n"], { lenient: LCR }]);
    cases.push([`${t} empty ${h} CR no LF`, type, [line + h + ":\rY: 1\r\n\r\n"]]);
    cases.push([`${t} empty ${h} CR no LF lenient`, type, [line + h + ":\rY: 1\r\n\r\n"], { lenient: LH }]);
    cases.push([`${t} empty ${h} folded`, type, [line + h + ":\r\n chunked\r\n\r\n"]]);
    cases.push([`${t} empty ${h} folded lenient`, type, [line + h + ":\r\n chunked\r\n\r\n"], { lenient: LH }]);
    cases.push([`${t} empty ${h} folded bytes`, type, [line + h + ":\r\n 5\r\n\r\n"], { bytes: true }]);
  }
  // Folds (lenient headers).
  cases.push([`${t} fold keeps whitespace`, type, [line + "X: a  \r\n  b \r\n\r\n"], { lenient: LH }]);
  cases.push([`${t} fold of Content-Length`, type, [line + "Content-Length: 5\r\n 6\r\n\r\n"], { lenient: LH }]);
  cases.push([`${t} fold of TE chunked`, type, [line + CH + " \r\n\r\n12345"], { lenient: LH }]);
  cases.push([`${t} fold of TE gzip, chunked`, type, [line + "Transfer-Encoding: gzip,\r\n chunked\r\n\r\n12345"], {
    lenient: LH,
  }]);
  cases.push([`${t} space after start line`, type, [line + " X: y\r\n\r\n"]]);
  cases.push([`${t} space after start line lenient`, type, [line + " X: y\r\n\r\n"], { lenient: LH }]);
  cases.push([`${t} CR without LF after value lenient`, type, [line + "X: a\rY: 1\r\n\r\n"], {
    lenient: HTTPParser.kLenientOptionalLFAfterCR,
  }]);
  // Chunk extensions.
  for (const [n, e] of [
    ["token", ";a=b;c"], ["quoted", ';a="b\\"c"'], ["empty names", ";;=x"], ["quote in token", ';a=b"c"'],
    ["space", "; a"], ["bare", ";"], ["space after name", ";a "], ["DEL in name", ";a\x7fb"],
    ["DEL in token", ";a=b\x7f"], ["DEL quoted", ';a="b\x7f"'], ["ctl quoted", ';a="\x01"'], ["tab quoted", ';a="\t"'],
    ["obs-text quoted", ';a="\x80\xff"'], ["obs-text token", ";a=\x80"], ["unterminated", ';a="b'],
    ["quoted-pair ctl", ';a="\\\x01"'], ["quoted-pair DEL", ';a="\\\x7f"'], ["after quote", ';a="b"c'],
    ["LF after name", ";a\n"], ["LF after value", ";a=b\n"], ["LF after quote", ';a="b"\n'], ["comma", ";a,b"],
  ]) {
    cases.push([`${t} ext ${n}`, type, [line + ext(e)]]);
    if (n.startsWith("LF")) cases.push([`${t} ext ${n} lenient`, type, [line + ext(e)], { lenient: LCR }]);
  }
  cases.push([`${t} ext bytes`, type, [line + ext(';a="b\\"c";d=e')], { bytes: true }]);
  cases.push([`${t} ext bad bytes`, type, [line + ext(';a="b\x7f"')], { bytes: true }]);
  cases.push([`${t} ext last chunk`, type, [line + CH + "\r\n0;a=\x01\r\n\r\n"]]);
  for (const [n, e] of [
    ["16384 name", ";" + "a".repeat(16384)], ["16385 name", ";" + "a".repeat(16385)],
    ["16384 quoted", ";" + "a".repeat(16381) + '="b"'], ["16385 quoted", ";" + "a".repeat(16382) + '="b"'],
    ["16385 then LF", ";" + "a".repeat(16385) + "\n"], ["16385 then bad", ";" + "a".repeat(16385) + "\x01"],
    ["two extensions", ";" + "a".repeat(9000) + ";" + "b".repeat(8000)],
  ]) {
    cases.push([`${t} ext limit ${n}`, type, [line + ext(e)], { lenient: n.endsWith("LF") ? LCR : 0 }]);
  }
  cases.push([`${t} ext limit across buffers`, type, [line + CH + "\r\n1;" + "a".repeat(9000), "a".repeat(8000) + "\r\nx\r\n0\r\n\r\n"]]);
  cases.push([`${t} ext limit per chunk`, type, [line + CH + "\r\n" + ("1;" + "a".repeat(9000) + "\r\nx\r\n").repeat(2) + "0\r\n\r\n"]]);
  // Trailers.
  const tr = (n) => Array.from({ length: n }, (_, i) => `T${i}: v\r\n`).join("");
  const body = line + CH + "A: 1\r\n\r\n1\r\nx\r\n0\r\n";
  for (const n of [2, 3]) cases.push([`${t} ${n} trailers maxHeaderPairs 4`, type, [body + tr(n) + "\r\n"], { pairs: 4 }]);
  cases.push([`${t} trailers maxHeaderPairs 4 bytes`, type, [body + tr(3) + "\r\n"], { pairs: 4, bytes: true }]);
  for (const max of [40, 60]) {
    cases.push([`${t} trailer under head limit ${max}`, type, [body + "Trailer-Name: " + "v".repeat(30) + "\r\n\r\n"], { max }]);
  }
  const next = line + "Content-Length: 0\r\n\r\n";
  for (const [n, h] of [
    ["Connection: close", "Connection: close"], ["Transfer-Encoding: gzip", "Transfer-Encoding: gzip"],
    ["Content-Length", "Content-Length: 5"], ["empty Transfer-Encoding", "Transfer-Encoding:"],
    ["Upgrade", "Upgrade: x\r\nConnection: upgrade"],
  ]) {
    cases.push([`${t} trailer ${n}`, type, [line + CH + "\r\n0\r\n" + h + "\r\n\r\n" + next]]);
  }
  // have_flushed_ is sticky.
  cases.push([`${t} after 33 fields`, type, [line + "Content-Length: 0\r\n" + tr(33) + "\r\n" + next + next]]);
  // The head limit at a buffer's end; an invalid byte first.
  cases.push([`${t} field name past limit at buffer end`, type, [line + "A: 1\r\nB" + "b".repeat(40), "b: 2\r\n\r\n"], { max: 40 }]);
  cases.push([`${t} field name past limit then bad byte`, type, [line + "A" + "a".repeat(40) + "\x01: x\r\n\r\n"], { max: 40 }]);
  cases.push([`${t} value counts trailing OWS`, type, [line + "A: " + "v".repeat(10) + " ".repeat(10) + "\r\n\r\n"], { max: 20 }]);
  // Body framing.
  cases.push([`${t} TE gzip beats CL`, type, [line + "Content-Length: 0\r\nTransfer-Encoding: gzip\r\n\r\nab"], {
    lenient: HTTPParser.kLenientChunkedLength,
  }]);
}
for (const [n, data] of [
  ["url past limit at buffer end", ["GET /" + "u".repeat(30), "x HTTP/1.1\r\n\r\n"]],
  ["url past limit at space", ["GET /" + "u".repeat(30) + " HTTP/1.1\r\n\r\n"]],
  ["url past limit then tab", ["GET /" + "u".repeat(30) + "\t HTTP/1.1\r\n\r\n"]],
  ["url past limit then ctl", ["GET /?" + "u".repeat(30) + "\x01 HTTP/1.1\r\n\r\n"]],
]) {
  cases.push([n, R, data, { max: 20 }]);
}
cases.push(["reason past limit at buffer end", S, ["HTTP/1.1 200 " + "r".repeat(50), "x"], { max: 20 }]);
for (const [n, data, opts] of [
  ["http09", ["GET /\r\n\r\n"]], ["http09 LF", ["GET /x?y\nA: b\r\n\r\nGET / HTTP/1.1\r\n\r\n"]],
  ["http09 body", ["POST /\r\nContent-Length: 2\r\n\r\nab"]], ["http09 bytes", ["GET /\r\n\r\n"], { bytes: true }],
  ["http09 CR then x", ["GET /\rx"]], ["http09 connect", ["CONNECT a:1\r\n\r\n"]],
  ["http09 space lenient", ["GET /\r\n X: y\r\n\r\n"], { lenient: LH }], ["empty url CR", ["GET \r\n\r\n"]],
  ["url form feed", ["GET /a\fb HTTP/1.1\r\n\r\n"]],
  ["request line bare LF lenient", ["GET / HTTP/1.1\nX: y\r\n\r\n"], { lenient: LCR }],
  ["request line CR without LF", ["GET / HTTP/1.1\rX: y\r\n\r\n"]],
  ["TE after chunked split", ["POST / HTTP/1.1\r\n" + CH + "Transfer-Encoding: g", "zip\r\n\r\n"]],
]) {
  cases.push([n, R, data, opts]);
}
for (const st of [100, 102, 103, 150, 199]) {
  cases.push([`res ${st} with CL`, S, [`HTTP/1.1 ${st} X\r\nContent-Length: 2\r\n\r\nab` + RS + "Content-Length: 0\r\n\r\n"]]);
}
cases.push(["res status bare LF", S, ["HTTP/1.1 200\nX: y\r\n\r\n"]]);
// Content-Length, Transfer-Encoding and Connection values are read a byte at
// a time as llhttp reads them, so a value split over buffers is judged as one
// (offsets in the buffer the byte is in), and an obs-fold goes on with the
// header state its line ended in.
for (const [t, type, line] of [["req", R, RQ], ["res", S, RS]]) {
  // Only SP may follow "chunked" (a tab makes it a plain token).
  cases.push([`${t} TE chunked then tab`, type, [line + "Transfer-Encoding: chunked\t\r\n\r\nabc"], { finish: 1 }]);
  cases.push([`${t} TE chunked then spaces`, type, [line + "Transfer-Encoding: chunked  \r\n\r\n0\r\n\r\n"]]);
  cases.push([`${t} TE chunked tab bare LF lenient`, type, [line + "Transfer-Encoding: chunked\t\n\r\n"], { lenient: LCR }]);
  cases.push([`${t} TE chunked twice`, type, [line + CH + CH + "\r\n0\r\n\r\n"]]);
  cases.push([`${t} TE chunked comma split`, type, [line + "Transfer-Encoding:chunked", ", gzip\r\n\r\n"]]);
  cases.push([`${t} TE gzip OWS chunked`, type, [line + "Transfer-Encoding: gzip ,\tchunked\r\n\r\n0\r\n\r\n"]]);
  cases.push([`${t} TE chunked bytes`, type, [line + "Transfer-Encoding: x, CHUNKED\r\n\r\n0\r\n\r\n"], { bytes: true }]);
  // Content-Length digits, then OWS, then the line end.
  cases.push([`${t} CL digit after OWS`, type, [line + "Content-Length: 1 2\n"]]);
  cases.push([`${t} CL ctl after letter`, type, [line + "Content-Length: 1x\x01"]]);
  cases.push([`${t} CL split`, type, [line + "Content-Length: 1", "2 \t", "x\r\n\r\n"]]);
  cases.push([`${t} CL overflow bytes`, type, [line + "Content-Length: 184467440737095516150\r\n\r\n"], { bytes: true }]);
  // The value is counted before its line end is judged.
  cases.push([`${t} value over limit then bare LF`, type, [line + "X: abcdef\nY: z\r\n\r\n"], { max: 8 }]);
  cases.push([`${t} value over limit then ctl`, type, [line + "X: abcdef\x01\r\n\r\n"], { max: 8 }]);
  // obs-fold carries the Connection token state over (lenient headers).
  for (const [n, h] of [
    ["x then close", "Connection: x\r\n close"], ["keep-alive then close", "Connection: keep-alive\r\n close"],
    ["x then upgrade", "Upgrade: ws\r\nConnection: x\r\n upgrade"], ["close then x", "Connection: close\r\n x"],
    ["Proxy-Connection close", "Proxy-Connection: close"], ["CLOSE, upgrade", "Upgrade: ws\r\nConnection: x, CLOSE ,\tUpgrade"],
    ["closex", "Connection: closex"], ["empty Upgrade", "Upgrade:\r\nConnection: upgrade"],
  ]) {
    cases.push([`${t} Connection ${n}`, type, [line + h + "\r\nContent-Length: 0\r\n\r\n"], { lenient: LH }]);
  }
  // A field line's first byte, and the bytes before its colon.
  cases.push([`${t} tab at first field`, type, [line + "\tX: y\r\n\r\n"]]);
  cases.push([`${t} bare LF at field start`, type, [line + "X: y\r\n\n"]]);
  cases.push([`${t} space before colon`, type, [line + "Content-Length : 1\r\n\r\nx"]]);
  cases.push([`${t} spaces before colon lenient`, type, [line + "Content-Length  : 1\r\n\r\nx"], { lenient: LH }]);
  // kOnHeadersComplete's answer, as node reads it.
  for (const v of [3, -1, 21, "2", 1.9, 2 ** 53 + 2, Infinity, null]) {
    cases.push([`${t} kOnHeadersComplete returns ${String(v)}`, type, [line + "Content-Length: 1\r\n\r\nx"], { hcRet: v }]);
  }
  cases.push([`${t} no kOnHeadersComplete`, type, [line + "X: y\r\nContent-Length: 1\r\n\r\nx"], { noHC: true }]);
  cases.push([`${t} no kOnHeadersComplete, trailers`, type, [line + CH + "X: y\r\n\r\n0\r\nT: z\r\n\r\n"], { noHC: true }]);
}
// The start line: HTTP, RTSP or ICE, each with its methods; a PRI request
// line is followed by the rest of the HTTP/2 preface; a request target is
// read by llhttp's url grammar (a CONNECT target as an authority).
for (const [n, data, opts] of [
  ["RTSP request", ["DESCRIBE / RTSP/1.0\r\n\r\n"]], ["RTSP method over HTTP", ["DESCRIBE / HTTP/1.1\r\n\r\n"], { bytes: true }],
  ["ICE request", ["SOURCE / ICE/1.0\r\n\r\n"]], ["ICE needs SOURCE", ["GET / ICE/1.0\r\n\r\n"]],
  ["bad protocol", ["GET / HTT:P/1.1\r\n\r\n"]], ["bad protocol bytes", ["GET / IX/1.1\r\n\r\n"], { bytes: true }],
  ["PRI preface", ["PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"]], ["PRI 1.1 waits", ["PRI * HTTP/1.1\r\n\r\n"]],
  ["PRI bad preface", ["PRI * HTTP/2.0\r\n\r\nXX"]],
  ["method then DEL", ["GET\x7f / HTTP/1.1\r\n\r\n"]], ["method prefix then DEL", ["GE\x7f / HTTP/1.1\r\n\r\n"]],
  ["url absolute", ["GET http://u@h:1/p?q#f HTTP/1.1\r\n\r\n"]], ["url schema only", ["GET http:/x HTTP/1.1\r\n\r\n"]],
  ["url no slash", ["GET x HTTP/1.1\r\n\r\n"]], ["url double at", ["GET http://u@@h/ HTTP/1.1\r\n\r\n"]],
  ["url obs-text", ["GET /a\x80 HTTP/1.1\r\n\r\n"]], ["CONNECT quote", ['CONNECT a":1 HTTP/1.1\r\n\r\n']],
  ["CONNECT path", ["CONNECT /x HTTP/1.1\r\n\r\n"]],
]) {
  cases.push([n, R, data, opts]);
}
cases.push(["res RTSP status line", S, ["RTSP/1.0 200 OK\r\n\r\n"]]);
cases.push(["res bad protocol bytes", S, ["RX/1.1 200 OK\r\n\r\n"], { bytes: true }]);
// Past a pause: a 101 that is no upgrade leaves the EOF unsafe until the
// next execute(); after an upgrade llhttp parses on when resumed, and node
// reports no parse error (nor a finish() one) while the upgrade flag is up.
cases.push(["res 101 then finish", S, ["HTTP/1.1 101 S\r\n\r\n"], { finish: 1 }]);
cases.push(["res 101 then more then finish", S, ["HTTP/1.1 101 S\r\n\r\n", "HTTP/1.1 200 OK\r\n\r\n"], { finish: 1 }]);
cases.push(["req upgrade then next request", R, ["CONNECT a:1 HTTP/1.1\r\n\r\n", "GET / HTTP/1.1\r\n\r\n"]]);
cases.push(["req upgrade then bad byte", R, ["CONNECT a:1 HTTP/1.1\r\n\r\n", "SX"], { finish: 1 }]);
cases.push(["req upgrade closed then byte", R, ["CONNECT a:1 HTTP/1.0\r\n\r\n", "\r\nx"]]);
// finish() on a body read to EOF completes the message each time and leaves
// the parse where it is.
cases.push(["res body to EOF finish twice", S, ["HTTP/1.1 200 OK\r\n\r\nab"], { finish: 2 }]);
cases.push(["req CONNECT with CL", R, ["CONNECT a:1 HTTP/1.1\r\nContent-Length: 2\r\n\r\nabcd"]]);

for (const [name, type, chunks, opts] of cases) {
  let line;
  try {
    line = run(type, chunks, opts);
  } catch (e) {
    line = "THREW " + e.message;
  }
  console.log(name + ": " + line);
}
