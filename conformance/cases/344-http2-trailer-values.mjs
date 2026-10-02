// The values of an http2 trailer section, as they reach the peer, both
// ways: node writes each one byte per UTF-16 unit (its low byte, so U+20AC
// arrives as 0xAC "¬"); the receiving nghttp2 drops a field whose value holds
// a control byte other than HTAB, or DEL, or starts or ends with SP or HTAB,
// and keeps the rest (an empty value too); and a NUL byte anywhere -- U+0100,
// an astral character -- leaves the section empty, its 'trailers' still
// emitted. sendTrailers() takes all of these without throwing. Measured on
// node v22.22.2.
//
// Regression guard: oam refused such a section natively. From the server
// the response was then never ended and the client hung; from the client the
// whole section was dropped without a word.
import http2 from "node:http2";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const sections = {
  latin1: { "x-euro": "€", "x-cafe": "café €", "x-ff": "ÿ", "x-low": "\udc41", "x-ok": "1" },
  dropped: { "x-nl": "a\nb", "x-cr": "a\rb", "x-c1": "a\x01b", "x-del": "a\x7fb", "x-lead": " a", "x-trail": "a\t", "x-tab": "a\tb", "x-mid": "a b", "x-empty": "", "x-arr": ["1", "a\nb", "3"], "x-ok": "1" },
  "all dropped": { "x-nl": "a\nb" },
  nul: { "x-a": "1", "x-nul": "a\0b", "x-z": "2" },
  "u+0100": { "x-a": "1", "x-b": "Ā" },
  astral: { "x-a": "1", "x-b": "\u{1F600}" },
};
const shown = (raw) => {
  const out = [];
  for (let i = 0; i < raw.length; i += 2) out.push(raw[i] + "=" + [...raw[i + 1]].map((c) => c.charCodeAt(0).toString(16)).join(","));
  return out.join(" ");
};

let serverLog = [];
let serverClosed = null;
const server = http2.createServer();
server.on("stream", (stream, headers) => {
  const [, dir, name] = headers[":path"].split("/");
  const section = sections[decodeURIComponent(name)];
  stream.on("trailers", (t, flags, raw) => serverLog.push(`trailers flags=${flags} ${shown(raw)}`));
  stream.on("close", () => {
    serverLog.push("close " + stream.rstCode);
    serverClosed();
  });
  stream.resume();
  stream.on("end", () => {
    if (dir === "s2c") {
      stream.respond({ ":status": 200 }, { waitForTrailers: true });
      stream.on("wantTrailers", () => {
        stream.sendTrailers(section);
        serverLog.push("sent " + Object.keys(stream.sentTrailers).length);
      });
      stream.end("ok");
    } else {
      stream.respond({ ":status": 200 });
      stream.end("ok");
    }
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const client = http2.connect("http://127.0.0.1:" + server.address().port);

for (const dir of ["s2c", "c2s"]) {
  for (const name of Object.keys(sections)) {
    const events = [];
    const closed = new Promise((r) => (serverClosed = r));
    await new Promise((resolve) => {
      const req = client.request({ ":method": "POST", ":path": `/${dir}/${encodeURIComponent(name)}` }, dir === "c2s" ? { waitForTrailers: true } : {});
      if (dir === "c2s") {
        req.on("wantTrailers", () => {
          req.sendTrailers(sections[name]);
          events.push("sent " + Object.keys(req.sentTrailers).length);
        });
      }
      req.on("trailers", (t, flags, raw) => events.push(`trailers flags=${flags} ${shown(raw)}`));
      req.on("error", (e) => events.push("error " + e.code));
      req.resume();
      req.on("end", () => events.push("end"));
      req.on("close", () => {
        events.push("close " + req.rstCode);
        resolve();
      });
      req.end("x");
    });
    await closed;
    console.log(dir, name);
    for (const e of events) console.log("  client " + e);
    for (const s of serverLog) console.log("  server " + s);
    serverLog = [];
  }
}
client.close();
server.close();
