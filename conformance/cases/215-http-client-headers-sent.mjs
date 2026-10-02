// http.ClientRequest's head is rendered by the first write(), by end() or
// by flushHeaders(), as node's OutgoingMessage renders it (`_header`):
// headersSent is true from then on, and setHeader / appendHeader /
// removeHeader throw ERR_HTTP_HEADERS_SENT. oam rendered it only when the
// request went out, so headersSent stayed false after write(), none of the
// three threw, and a header set after a write() in the same tick still
// reached the server. Measured on node v22.22.2.
import http from "node:http";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const server = http.createServer((req, res) => {
  req.resume();
  req.on("end", () => res.end(JSON.stringify({ late: req.headers["x-late"] ?? null, late2: req.headers["x-late2"] ?? null })));
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

class SocketAgent extends http.Agent {
  createConnection(options, cb) {
    return super.createConnection(options, cb);
  }
}

const tryIt = (fn) => {
  try {
    fn();
    return "ok";
  } catch (e) {
    return e.code;
  }
};

async function run(label, agent, commit, later) {
  const req = http.request({ host: "127.0.0.1", port, method: "POST", path: "/", agent });
  const before = req.headersSent;
  commit(req);
  const report = () =>
    `set=${tryIt(() => req.setHeader("x-late", "1"))} append=${tryIt(() => req.appendHeader("x-late2", "2"))} remove=${tryIt(() => req.removeHeader("x-gone"))}`;
  let now = "";
  if (!later) now = report();
  const body = await new Promise((resolve) => {
    req.on("response", (res) => {
      let text = "";
      res.setEncoding("utf8");
      res.on("data", (c) => (text += c));
      res.on("end", () => resolve(text));
    });
    req.on("error", (e) => resolve(`error ${e.code}`));
    if (later) {
      setTimeout(() => {
        now = report();
        req.end();
      }, 50);
    } else if (!req.writableEnded) {
      req.end();
    }
  });
  console.log(`${label}: headersSent ${before} -> ${req.headersSent}, ${now}, server saw ${body}`);
  if (agent) agent.destroy();
}

for (const [name, makeAgent] of [
  ["default agent", () => undefined],
  ["keepAlive agent", () => new http.Agent({ keepAlive: true })],
  ["agent socket", () => new SocketAgent()],
]) {
  await run(`${name}, after write()`, makeAgent(), (req) => req.write("abc"), false);
  await run(`${name}, 50 ms after write()`, makeAgent(), (req) => req.write("abc"), true);
  await run(`${name}, after flushHeaders()`, makeAgent(), (req) => req.flushHeaders(), false);
  await run(`${name}, before any write`, makeAgent(), () => {}, false);
}
// appendHeader before the head: a list, sent as node sends it.
{
  const req = http.request({ host: "127.0.0.1", port, method: "POST", path: "/" });
  req.appendHeader("x-late", "a");
  req.appendHeader("x-late", ["b", "c"]);
  console.log("appendHeader before the head:", JSON.stringify(req.getHeader("x-late")));
  const body = await new Promise((resolve) => {
    req.on("response", (res) => {
      let text = "";
      res.on("data", (c) => (text += c));
      res.on("end", () => resolve(text));
    });
    req.end();
  });
  console.log("server saw", body);
}
server.close();
