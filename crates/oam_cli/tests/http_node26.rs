//! node v26.13.1's http / https / http2 behaviour where it moved from v22:
//! each script's expected output is node v26.13.1's own, measured by running
//! the same script under it.

use std::path::PathBuf;
use std::process::Command;

fn write_temp(name: &str, content: &str) -> PathBuf {
    use std::sync::OnceLock;
    static RUN_DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = RUN_DIR.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("oam-http-node26-{}-{nanos}", std::process::id()))
    });
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

/// `oam run <script> --no-check` with the cache in this run's temp directory
/// and `NODE_OPTIONS` cleared; stdout with CRLF folded, once it exits 0.
fn run(name: &str, script: &str) -> String {
    let path = write_temp(name, script);
    let cache = write_temp("oam-cache/.keep", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let out = Command::new(env!("CARGO_BIN_EXE_oam"))
        .args(["run", path.to_str().unwrap(), "--no-check"])
        .env("OAM_CACHE_DIR", &cache)
        .env("NO_COLOR", "1")
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("oam runs");
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    assert!(
        out.status.success(),
        "{name} exited {}\nstdout: {stdout}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    stdout.trim_end().to_string()
}

/// A response's writes in one tick go out as one chunk (node v26's write_ corks the message until the next tick), on oam's own server and over an assigned socket alike; writes a turn apart stay separate chunks, and a sized body is unframed.
#[test]
fn a_tick_of_response_writes_goes_out_as_one_chunk() {
    let stdout = run(
        "chunks.mjs",
        r##"import http from "node:http";
import net from "node:net";
import { Writable } from "node:stream";
setTimeout(() => { console.log("WATCHDOG"); process.exit(9); }, 20000).unref();
const server = http.createServer((req, res) => {
  if (req.url === "/tick") {
    res.write("a");
    res.write("b");
    res.end("c");
  } else if (req.url === "/turns") {
    res.write("a");
    setImmediate(() => {
      res.write("b");
      setImmediate(() => res.end());
    });
  } else {
    res.setHeader("content-length", "2");
    res.write("x");
    res.end("y");
  }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
for (const path of ["/tick", "/turns", "/sized"]) {
  const raw = await new Promise((resolve) => {
    const c = net.connect(server.address().port, "127.0.0.1", () => {
      c.write(`GET ${path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n`);
    });
    let data = "";
    c.setEncoding("latin1");
    c.on("data", (d) => (data += d));
    c.on("end", () => resolve(data));
  });
  console.log(path, JSON.stringify(raw.slice(raw.indexOf("\r\n\r\n") + 4)));
}
server.close();
const req = new http.IncomingMessage(new net.Socket());
req.httpVersionMajor = 1;
req.httpVersionMinor = 1;
const res = new http.ServerResponse(req);
res.sendDate = false;
const out = [];
res.assignSocket(new Writable({ write(c, e, cb) { out.push(Buffer.from(c, e)); cb(); } }));
res.write("a");
res.write("b");
res.end("c");
await new Promise((r) => setImmediate(r));
console.log("assigned socket", JSON.stringify(Buffer.concat(out).toString("latin1")));
"##,
    );
    let expected = r##"/tick "3\r\nabc\r\n0\r\n\r\n"
/turns "1\r\na\r\n1\r\nb\r\n0\r\n\r\n"
/sized "xy"
assigned socket "HTTP/1.1 200 OK\r\nConnection: keep-alive\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n""##;
    assert_eq!(stdout, expected);
}

/// end()'s callback is called with null just ahead of 'finish', client and server, with writableFinished false until then; a ClientRequest whose connection is refused hands it the socket's error and never finishes.
#[test]
fn end_callbacks_hear_null_ahead_of_finish_or_the_failure() {
    let stdout = run(
        "endcb.mjs",
        r##"import http from "node:http";
import net from "node:net";
setTimeout(() => { console.log("WATCHDOG"); process.exit(9); }, 20000).unref();
const server = http.createServer((req, res) => {
  const ev = [];
  res.on("finish", () => ev.push(`finish writableFinished=${res.writableFinished}`));
  req.resume();
  req.on("end", () => {
    res.end("ok", function () {
      ev.push(`end callback args=${arguments.length} ${arguments[0]}`);
    });
    ev.push(`after end() writableFinished=${res.writableFinished}`);
    res.on("close", () => console.log("server:", ev.join(", ")));
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
await new Promise((resolve) => {
  const ev = [];
  const req = http.request({ host: "127.0.0.1", port, method: "POST", agent: false }, (res) => {
    res.resume();
    res.on("end", () => setTimeout(() => { console.log("client:", ev.join(", ")); resolve(); }, 50));
  });
  req.on("finish", () => ev.push("finish"));
  req.write("a", (err) => ev.push(`write callback ${err}`));
  req.end("b", function () { ev.push(`end callback args=${arguments.length} ${arguments[0]}`); });
});
const gone = net.createServer();
await new Promise((r) => gone.listen(0, "127.0.0.1", r));
const closed = gone.address().port;
await new Promise((r) => gone.close(r));
await new Promise((resolve) => {
  const ev = [];
  const req = http.request({ host: "127.0.0.1", port: closed, method: "POST", agent: false });
  req.on("finish", () => ev.push("finish"));
  req.on("error", (e) => ev.push(`error ${e.code}`));
  req.on("close", () => setTimeout(() => { console.log("refused:", ev.join(", ")); resolve(); }, 100));
  req.end("b", (err) => ev.push(`end callback ${err && err.code}`));
});
server.close();
"##,
    );
    let expected = r##"server: after end() writableFinished=false, end callback args=1 null, finish writableFinished=true
client: write callback null, end callback args=1 null, finish
refused: error ECONNREFUSED, end callback ERR_SOCKET_CLOSED_BEFORE_CONNECTION"##;
    assert_eq!(stdout, expected);
}

/// node v26's new http surface: isValidHeaderName / isValidHeaderValue, websocketMask / websocketUnmask, _http_common's calculateLenientFlags and kLenientHeaderValueRelaxed parsing, _http_incoming's raw-header helpers, the httpValidation option's checks, _isLenientHeaderValidation and the ClientRequest#path accessor.
#[test]
fn http_v26_helper_surface() {
    let stdout = run(
        "helpers.cjs",
        r##"const http = require("http");
const net = require("net");
const t = (f) => { try { return JSON.stringify(f()); } catch (e) { return `${e.name} ${e.code}`; } };
console.log(["isValidHeaderName", "isValidHeaderValue", "websocketMask", "websocketUnmask"].map((n) => `${n}/${http[n].length}`).join(" "));
console.log(require("_http_outgoing").isValidHeaderValue === http.isValidHeaderValue);
console.log(["x-a", "", "a b", 1].map((v) => http.isValidHeaderName(v)).join(" "));
console.log(["a", "a\x01", "a\x00", "€", undefined, null].map((v) => `${http.isValidHeaderValue(v)}/${http.isValidHeaderValue(v, { httpValidation: "relaxed" })}`).join(" "));
console.log(t(() => http.isValidHeaderValue("a", null)), t(() => http.isValidHeaderValue("a", { httpValidation: "insecure" })));
const out = Buffer.alloc(8);
http.websocketMask(Buffer.from([1, 2, 3, 4, 5, 6]), Buffer.from([0xaa, 0xbb, 0xcc, 0xdd]), out, 1, 6);
console.log(out.toString("hex"));
const u = Buffer.from(out.subarray(1, 7));
http.websocketUnmask(u, new Uint8Array([0xaa, 0xbb, 0xcc, 0xdd]));
console.log(u.toString("hex"));
console.log(t(() => http.websocketMask(Buffer.alloc(2), Buffer.alloc(3), out, 0, 2)), t(() => http.websocketMask(Buffer.alloc(2), Buffer.alloc(4), out, 7, 2)), t(() => http.websocketUnmask("x", Buffer.alloc(4))));
const common = require("_http_common");
console.log(common.HTTPParser.kLenientHeaderValueRelaxed, common.HTTPParser.kLenientAll, ["strict", "relaxed", "insecure"].map((v) => common.calculateLenientFlags(v)).join(" "), common.calculateLenientFlags(undefined, true), common.calculateLenientFlags(undefined, false));
console.log(common._checkInvalidHeaderChar("a\x01"), common._checkInvalidHeaderChar("a\x01", true), common._checkInvalidHeaderChar("a\x00", true));
for (const flags of [0, common.HTTPParser.kLenientHeaderValueRelaxed]) {
  for (const c of ["\x01", "\x7f", "\x00"]) {
    const p = new common.HTTPParser();
    let got = "none";
    p.initialize(common.HTTPParser.REQUEST, {}, 0, flags);
    p[common.HTTPParser.kOnHeadersComplete] = (ma, mi, headers) => (got = JSON.stringify(headers[3]));
    const r = p.execute(Buffer.from(`GET / HTTP/1.1\r\nHost: a\r\nX: a${c}b\r\n\r\n`, "latin1"));
    console.log(flags, JSON.stringify(c), r instanceof Error ? r.code : got);
  }
}
const inc = require("_http_incoming");
console.log(typeof inc.kHeadersCount, typeof inc.kDetachAbortSignal, inc.asciiEqualIgnoreCase("Content-Type", "content-type"), inc.asciiEqualIgnoreCase("Content-Typo", "content-type"));
const m = new http.IncomingMessage(new net.Socket());
m._addHeaderLines(["TE", "chunked", "te", "trailers", "X", "1"], 4);
console.log(m[inc.kHeadersCount], inc.getRawHeader(m, "te", true), inc.getRawHeader(m, "te", false), inc.getRawHeader(m, "x", true));
console.log(t(() => http.createServer({ httpValidation: "x" })), t(() => http.createServer({ httpValidation: "strict", insecureHTTPParser: false })), http.createServer({ httpValidation: "relaxed" }).httpValidation);
const reqs = [undefined, "strict", "relaxed"].map((httpValidation) => {
  const r = http.request({ host: "127.0.0.1", port: 9, httpValidation });
  r.on("error", () => {});
  return r;
});
console.log(reqs.map((r) => `${r.httpValidation}:${r._isLenientHeaderValidation()}`).join(" "));
const d = Object.getOwnPropertyDescriptor(http.ClientRequest.prototype, "path");
console.log(typeof d.get, typeof d.set, d.enumerable, Object.keys(reqs[0]).includes("path"), reqs[0].path, t(() => { reqs[0].path = "/a b"; }), reqs[0].path);
reqs.forEach((r) => r.destroy());
"##,
    );
    let expected = r##"isValidHeaderName/1 isValidHeaderValue/2 websocketMask/5 websocketUnmask/2
true
true false false false
true/true false/true false/false false/false false/false true/true
TypeError ERR_INVALID_ARG_TYPE TypeError ERR_INVALID_ARG_VALUE
00abb9cfd9afbd00
010203040506
TypeError ERR_INVALID_ARG_VALUE RangeError ERR_OUT_OF_RANGE TypeError ERR_INVALID_ARG_TYPE
1024 1023 0 1024 1023 1023 0
true false true
0 "\u0001" HPE_INVALID_HEADER_TOKEN
0 "" HPE_INVALID_HEADER_TOKEN
0 "\u0000" HPE_INVALID_HEADER_TOKEN
1024 "\u0001" "a\u0001b"
1024 "" "ab"
1024 "\u0000" HPE_INVALID_HEADER_TOKEN
symbol symbol true false
4 chunked, trailers chunked undefined
TypeError ERR_INVALID_ARG_VALUE TypeError ERR_INVALID_ARG_VALUE relaxed
undefined:false strict:false relaxed:true
function function true false / TypeError ERR_UNESCAPED_CHARACTERS /"##;
    assert_eq!(stdout, expected);
}

/// IncomingMessage#signal: aborted when the client goes before the request is answered, never for an exchange that completed, and at once on a message already destroyed unread.
#[test]
fn incoming_message_signal_aborts_only_a_message_cut_short() {
    let stdout = run(
        "signal.mjs",
        r##"import http from "node:http";
import net from "node:net";
setTimeout(() => { console.log("WATCHDOG"); process.exit(9); }, 20000).unref();
const lines = [];
const server = http.createServer((req, res) => {
  const signal = req.signal;
  lines.push(`${req.url} ${signal.constructor.name} aborted=${signal.aborted} same=${signal === req.signal}`);
  signal.addEventListener("abort", () => lines.push(`${req.url} abort ${signal.reason.name}`));
  if (req.url === "/answered") res.end("ok");
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
await new Promise((r) => http.get({ port, path: "/answered", agent: false }, (res) => {
  lines.push(`client aborted=${res.signal.aborted}`);
  res.signal.addEventListener("abort", () => lines.push("client abort"));
  res.resume();
  res.on("end", () => setTimeout(r, 100));
}));
await new Promise((r) => {
  const c = net.connect(port, "127.0.0.1", () => {
    c.write("GET /dropped HTTP/1.1\r\nHost: x\r\n\r\n");
    setTimeout(() => { c.destroy(); setTimeout(r, 200); }, 100);
  });
});
const bare = new http.IncomingMessage(new net.Socket());
bare.destroy();
lines.push(`destroyed before read: aborted=${bare.signal.aborted}`);
console.log(lines.join("\n"));
server.close();
"##,
    );
    let expected = r##"/answered AbortSignal aborted=false same=true
client aborted=false
/dropped AbortSignal aborted=false same=true
/dropped abort AbortError
destroyed before read: aborted=true"##;
    assert_eq!(stdout, expected);
}

/// http2: the strictSingleValueFields session option (validated, default true; false lets a single-value field repeat), request() on a closed session failing the stream it returns on the next tick, and the compatibility response's writeInformation checks and writable getters.
#[test]
fn http2_strict_single_value_fields_and_requests_on_a_closed_session() {
    let stdout = run(
        "h2.mjs",
        r##"import http2 from "node:http2";
setTimeout(() => { console.log("WATCHDOG"); process.exit(9); }, 20000).unref();
const t = (f) => { try { return String(f()); } catch (e) { return `${e.name} ${e.code}`; } };
console.log(t(() => http2.connect("http://127.0.0.1:9", { strictSingleValueFields: "no" })));
const server = http2.createServer((req, res) => {
  console.log("writeInformation", t(() => res.writeInformation(101)), t(() => res.writeInformation(200)), typeof res.writableNeedDrain, res.writableObjectMode);
  res.end("ok");
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const url = "http://127.0.0.1:" + server.address().port;
for (const strict of [undefined, false]) {
  const session = http2.connect(url, strict === undefined ? {} : { strictSingleValueFields: strict });
  await new Promise((resolve) => {
    let req;
    try {
      req = session.request({ ":path": "/", "content-type": ["a", "b"] });
    } catch (e) {
      console.log(`strict=${strict}: throws ${e.code}`);
      resolve();
      return;
    }
    console.log(`strict=${strict}: sent`);
    req.resume();
    req.on("close", resolve);
    req.end();
  });
  session.close();
  await new Promise((r) => session.on("close", r));
  const late = session.request({ ":path": "/" });
  console.log("request on a closed session: returned", late.constructor.name);
  await new Promise((r) => late.on("error", (e) => { console.log("its error", e.code); r(); }));
}
server.close();
"##,
    );
    let expected = r##"TypeError ERR_INVALID_ARG_TYPE
strict=undefined: throws ERR_HTTP2_HEADER_SINGLE_VALUE
request on a closed session: returned ClientHttp2Stream
its error ERR_HTTP2_INVALID_SESSION
strict=false: sent
writeInformation RangeError ERR_HTTP2_STATUS_INVALID RangeError ERR_HTTP2_STATUS_INVALID boolean false
request on a closed session: returned ClientHttp2Stream
its error ERR_HTTP2_INVALID_SESSION"##;
    assert_eq!(stdout, expected);
}
