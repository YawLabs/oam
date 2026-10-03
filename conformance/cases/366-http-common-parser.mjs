// node:_http_common's HTTPParser -- the class nock 14's interceptors
// (@mswjs/interceptors) parse every intercepted request and response with
// -- as node's llhttp binding drives it: which callback runs when and with
// which arguments (kOnMessageBegin at a message's first byte, kOnHeaders
// with fields in batches once a head has 32 or more and with a chunked
// body's trailers, kOnHeadersComplete's nine arguments and its return value,
// kOnBody per slice, kOnMessageComplete), what execute() returns -- the
// bytes parsed, a head's length on an upgrade, CONNECT or 101, or the parse
// error with node's code, reason and offset -- and finish(). Each input is
// also fed split across execute() calls. oam had no _http_common at all, so
// `import nock` failed (#207).
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const { HTTPParser, methods } = require("_http_common");

const names = ["kOnMessageBegin", "kOnHeaders", "kOnHeadersComplete", "kOnBody", "kOnMessageComplete"];
const show = (v) =>
  JSON.stringify(v, (k, x) => (x === undefined ? "U" : Buffer.isBuffer(x) ? "B:" + x.toString("latin1") : x));

function run(type, chunks, opts = {}) {
  const out = [];
  const p = new HTTPParser();
  if (opts.maxHeaderSize !== undefined) p.initialize(type, {}, opts.maxHeaderSize, opts.lenient || 0);
  else p.initialize(type, {});
  for (const n of names) {
    p[HTTPParser[n]] = function (...args) {
      out.push(n.slice(3) + show(args));
      if (n === "kOnHeadersComplete" && opts.ret !== undefined) return opts.ret;
    };
  }
  for (const c of chunks) {
    const r = p.execute(Buffer.from(c, "latin1"));
    if (r instanceof Error) {
      out.push("ERR " + show({ code: r.code, reason: r.reason, at: r.bytesParsed, message: r.message, keys: Object.keys(r) }));
      break;
    }
    out.push("ret " + r);
  }
  const f = p.finish();
  out.push("finish " + (f instanceof Error ? show({ code: f.code, reason: f.reason, at: f.bytesParsed }) : show(f)));
  return out.join(" | ");
}

const R = HTTPParser.REQUEST;
const S = HTTPParser.RESPONSE;
const heads = (n) => Array.from({ length: n }, (_, i) => "H" + i + ": " + i + "\r\n").join("");
const cases = [
  ["req simple", R, ["GET / HTTP/1.1\r\nHost: a\r\n\r\n"]],
  ["req split head", R, ["GE", "T /x HTTP/1.1\r", "\nHo", "st: a\r\n", "\r", "\n"]],
  ["req body CL", R, ["POST /p HTTP/1.1\r\nContent-Length: 5\r\n\r\nhel", "lo"]],
  ["req chunked", R, ["POST /p HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\n\r\n"]],
  ["req chunked trailers", R, ["POST /p HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n0\r\nX-T: 1\r\nY: 2\r\n\r\n"]],
  ["req chunked split", R, ["POST /p HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n", "A\r\n01234", "56789\r", "\n0\r\n", "\r\n"]],
  ["req pipelined", R, ["GET /1 HTTP/1.1\r\n\r\nGET /2 HTTP/1.1\r\n\r\n"]],
  ["req http10", R, ["GET / HTTP/1.0\r\n\r\n"]],
  ["req http10 keep-alive", R, ["GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"]],
  ["req after close", R, ["GET / HTTP/1.1\r\nConnection: close\r\n\r\n", "GET / HTTP/1.1\r\n\r\n"]],
  ["req after close crlf", R, ["GET / HTTP/1.1\r\nConnection: close\r\n\r\n", "\r\n"]],
  ["req leading crlf", R, ["\r\n\r\nGET / HTTP/1.1\r\n\r\n"]],
  ["req bad method", R, ["FOO / HTTP/1.1\r\n\r\n"]],
  ["req lowercase method", R, ["get / HTTP/1.1\r\n\r\n"]],
  ["req methods", R, ["DELETE / HTTP/1.1\r\n\r\nM-SEARCH * HTTP/1.1\r\n\r\nQUERY / HTTP/1.1\r\n\r\nPATCH / HTTP/1.1\r\n\r\n"]],
  ["req rtsp method", R, ["DESCRIBE / HTTP/1.1\r\n\r\n"]],
  ["req bad version", R, ["GET / HTTP/1.2\r\n\r\n"]],
  ["req version 2.0", R, ["GET / HTTP/2.0\r\n\r\n"]],
  ["req header token", R, ["GET / HTTP/1.1\r\nBad Header: x\r\n\r\n"]],
  ["req header ows", R, ["GET / HTTP/1.1\r\nA:   spaced  \t\r\nB:\r\nC:x\r\n\r\n"]],
  ["req header ctl", R, ["GET / HTTP/1.1\r\nA: x\x01y\r\n\r\n"]],
  ["req header latin1", R, ["GET / HTTP/1.1\r\nA: caf\xe9\r\n\r\n"]],
  ["req bare lf", R, ["GET / HTTP/1.1\r\nHost: a\n\r\n"]],
  ["req obs fold", R, ["GET / HTTP/1.1\r\nA: x\r\n  y\r\n\r\n"]],
  ["req dup CL", R, ["POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\na"]],
  ["req CL then TE", R, ["POST / HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"]],
  ["req TE then CL", R, ["POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 1\r\n\r\n0\r\n\r\n"]],
  ["req TE gzip", R, ["POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n"]],
  ["req TE chunked, gzip", R, ["POST / HTTP/1.1\r\nTransfer-Encoding: chunked, gzip\r\n\r\n"]],
  ["req TE gzip, chunked", R, ["POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n"]],
  ["req bad CL", R, ["POST / HTTP/1.1\r\nContent-Length: 1 2\r\n\r\n"]],
  ["req CL overflow", R, ["POST / HTTP/1.1\r\nContent-Length: 99999999999999999999\r\n\r\n"]],
  ["req bad chunk size", R, ["POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n"]],
  ["req chunk size overflow", R, ["POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffffff\r\n"]],
  ["req chunk data no crlf", R, ["POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcX0\r\n\r\n"]],
  ["req url ctl", R, ["GET /a\x01b HTTP/1.1\r\n\r\n"]],
  ["req url query ctl", R, ["GET /a?b\x01 HTTP/1.1\r\n\r\n"]],
  ["req connect", R, ["CONNECT h:443 HTTP/1.1\r\nHost: h:443\r\n\r\nrest", "more"]],
  ["req upgrade", R, ["GET / HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\nrest", "more"]],
  ["req upgrade header only", R, ["GET / HTTP/1.1\r\nUpgrade: websocket\r\n\r\n"]],
  ["req 40 headers", R, ["GET / HTTP/1.1\r\n" + heads(40) + "\r\n"]],
  ["req 33 headers split", R, ["GET / HTTP/1.1\r\n" + heads(33), "\r\n"]],
  ["req incomplete eof", R, ["GET / HTTP/1.1\r\nHost"]],
  ["req incomplete body eof", R, ["POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nab"]],
  ["req max header size", R, ["GET /" + "a".repeat(9) + " HTTP/1.1\r\nX: " + "b".repeat(9) + "\r\n\r\n"], { maxHeaderSize: 20 }],
  ["req under max header size", R, ["GET /" + "a".repeat(9) + " HTTP/1.1\r\nX: " + "b".repeat(9) + "\r\n\r\n"], { maxHeaderSize: 21 }],
  ["req lenient headers", R, ["GET / HTTP/1.1\r\nA: x\x01y\r\n\r\n"], { maxHeaderSize: 16384, lenient: HTTPParser.kLenientHeaders }],
  ["req skip body", R, ["POST / HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc"], { ret: 1 }],
  ["req upgrade by return", R, ["POST / HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc", "more"], { ret: 2 }],
  ["res simple", S, ["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi"]],
  ["res no reason", S, ["HTTP/1.1 204\r\n\r\n"]],
  ["res eof body", S, ["HTTP/1.1 200 OK\r\n\r\nsome", " more"]],
  ["res chunked", S, ["HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n0\r\n\r\n"]],
  ["res 304 with CL", S, ["HTTP/1.1 304 Not Modified\r\nContent-Length: 10\r\n\r\n"]],
  ["res 100 then 200", S, ["HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"]],
  ["res 101", S, ["HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\nframes"]],
  ["res head skip", S, ["HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n"], { ret: 1 }],
  ["res bad status", S, ["HTTP/1.1 2x0 OK\r\n\r\n"]],
  ["res bad proto", S, ["HTTZ/1.1 200 OK\r\n\r\n"]],
  ["res status 1000", S, ["HTTP/1.1 1000 Odd\r\n\r\n"]],
  ["res http10 keep-alive", S, ["HTTP/1.0 200 OK\r\nConnection: keep-alive\r\nContent-Length: 0\r\n\r\n"]],
  ["res TE gzip", S, ["HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\nraw"]],
  ["res keep-alive two", S, ["HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\naHTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n"]],
  ["res incomplete chunked eof", S, ["HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nh"]],
  ["res empty", S, []],
];
for (const [name, type, chunks, opts] of cases) {
  let line;
  try {
    line = run(type, chunks, opts);
  } catch (e) {
    line = "THREW " + e.message;
  }
  console.log(name + ": " + line);
}

// The constants and the method table.
const constants = Object.keys(HTTPParser).filter((k) => /^(k|REQUEST|RESPONSE)/.test(k)).sort();
console.log(constants.map((k) => k + "=" + HTTPParser[k]).join(" "));
console.log("methods " + methods.join(","));
console.log("own properties of a parser: " + Object.getOwnPropertyNames(new HTTPParser()).length);
