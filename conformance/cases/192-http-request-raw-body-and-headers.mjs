// http.request / http.get put on the wire what the program wrote and hand
// back what the server sent (measured on node v22.22.2): the request carries
// `host` and `connection` plus the caller's own headers -- no `accept`, no
// `user-agent`, no `accept-encoding` -- and a response with a
// `content-encoding` arrives as the encoded bytes, `content-encoding` and
// `content-length` intact, for the program to decode itself. A 3xx is the
// response; its Location is never requested.
//
// oam's own transport is the one fetch() uses, and it used to give
// http.request fetch's behaviour: three added request headers, and a body
// decoded behind the caller's back with both headers removed, so anything
// piping the response through zlib.createGunzip() failed on plain text.
// fetch() itself keeps negotiating and decoding (last lines).
//
// The compressed bodies are fixed bytes, not this runtime's zlib output, so
// both runtimes serve the same thing.
import http from "node:http";
import zlib from "node:zlib";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const TEXT = "hello-raw-body hello-raw-body hello-raw-body";
const BODIES = {
  gzip: Buffer.from("H4sIAAAAAAAACstIzcnJ1y1KLNdNyk+pVMjAxwUAJk2LbCwAAAA=", "base64"),
  deflate: Buffer.from("eJzLSM3JydctSizXTcpPqVTIwMcFAHIBEHM=", "base64"),
  br: Buffer.from("GysA+MVtbF3H8Y6ioCEInSiYCgH6tGRuHNP1AA==", "base64"),
};

const hits = [];
const server = http.createServer((req, res) => {
  const headers = { ...req.headers };
  // The one value that differs run to run.
  if (headers.host) headers.host = headers.host.replace(/:\d+$/, ":PORT");
  let n = 0;
  req.on("data", (d) => (n += d.length));
  req.on("end", () => {
    hits.push(`${req.method} ${req.url} ${JSON.stringify(headers)} body=${n}`);
    const coding = req.url.slice(1);
    if (BODIES[coding]) {
      res.writeHead(200, {
        "content-type": "text/plain",
        "content-encoding": coding,
        "content-length": BODIES[coding].length,
      });
      res.end(BODIES[coding]);
    } else if (req.url === "/redirect") {
      res.writeHead(302, { location: "/final", "content-length": 5 });
      res.end("moved");
    } else {
      res.end("plain");
    }
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

// An agent whose createConnection is the stock one called through a
// subclass: the request goes over the socket it returns.
class WrappingAgent extends http.Agent {
  createConnection(options, cb) {
    return super.createConnection(options, cb);
  }
}

function request(options, body) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: "127.0.0.1", port, ...options }, (res) => {
      const chunks = [];
      res.on("data", (d) => chunks.push(d));
      res.on("end", () => resolve({ res, raw: Buffer.concat(chunks) }));
      res.on("error", reject);
    });
    req.on("error", reject);
    req.end(body);
  });
}

async function run(label, options, body) {
  hits.length = 0;
  let line;
  try {
    const { res, raw } = await request(options, body);
    line = `${res.statusCode} ce=${res.headers["content-encoding"]} cl=${res.headers["content-length"]} ` +
      `len=${raw.length} body=${raw.length > 12 ? raw.toString("hex") : JSON.stringify(raw.toString())}`;
  } catch (e) {
    line = `ERROR ${e.code} ${e.message}`;
  }
  console.log(`${label}: ${line}`);
  for (const hit of hits) console.log(`  server saw ${hit}`);
}

const paths = [
  ["own transport", () => ({})],
  ["agent: false", () => ({ agent: false })],
  ["keep-alive agent", () => ({ agent: new http.Agent({ keepAlive: true }) })],
  ["agent socket", () => ({ agent: new WrappingAgent() })],
];
for (const [name, extra] of paths) {
  const agents = [];
  const opts = (o) => {
    const merged = { ...extra(), ...o };
    if (merged.agent) agents.push(merged.agent);
    return merged;
  };
  await run(`${name} GET /plain`, opts({ path: "/plain" }));
  await run(`${name} GET /gzip`, opts({ path: "/gzip" }));
  await run(`${name} GET /deflate`, opts({ path: "/deflate" }));
  await run(`${name} GET /br`, opts({ path: "/br" }));
  await run(`${name} HEAD /gzip`, opts({ path: "/gzip", method: "HEAD" }));
  await run(`${name} GET /redirect`, opts({ path: "/redirect" }));
  await run(`${name} POST /plain`, opts({ path: "/plain", method: "POST" }), "payload");
  // The caller's own negotiation headers go out as written, and the body
  // still comes back encoded.
  await run(`${name} GET /gzip, own headers`, opts({
    path: "/gzip",
    headers: { "Accept-Encoding": "gzip", "User-Agent": "case-192", Accept: "text/plain" },
  }));
  for (const agent of agents) agent.destroy();
}

// The program decodes for itself.
for (const [name, extra] of paths) {
  const options = { path: "/gzip", ...extra() };
  const text = await new Promise((resolve) => {
    const req = http.get({ host: "127.0.0.1", port, ...options }, (res) => {
      let out = "";
      const gunzip = zlib.createGunzip();
      gunzip.on("data", (d) => (out += d));
      gunzip.on("end", () => resolve(out));
      gunzip.on("error", (e) => resolve(`GUNZIP ERROR ${e.code}`));
      res.pipe(gunzip);
    });
    req.on("error", (e) => resolve(`ERROR ${e.code}`));
  });
  if (options.agent) options.agent.destroy();
  console.log(`${name} piped through createGunzip: ${text === TEXT ? "the text" : text}`);
}

// fetch() is the Fetch client: it decodes.
{
  hits.length = 0;
  const res = await fetch(`http://127.0.0.1:${port}/gzip`);
  const text = await res.text();
  console.log(`fetch /gzip: ${res.status} body is the text: ${text === TEXT}`);
  const sent = JSON.parse(hits[0].slice(hits[0].indexOf("{"), hits[0].lastIndexOf("}") + 1));
  console.log(`  fetch sent accept-encoding: ${"accept-encoding" in sent}, accept: ${sent.accept}`);
}

server.close();
