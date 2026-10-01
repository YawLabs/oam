// A ClientRequest's 'finish' and write() callbacks with no agent socket
// involved -- the default agent, which on oam is its own transport --
// measured on node v22.22.2. node emits 'finish' once the socket has
// written the request, so a request whose connection is refused gets none,
// req.writableFinished stays false, and its write() callbacks hear
// ERR_SOCKET_CLOSED_BEFORE_CONNECTION after 'close'. oam emitted 'finish' a
// tick after end() whatever became of the connection, with
// req.writableFinished true and every callback called without an error:
// code that takes 'finish' for "the request left" counted one that never
// did (#193).
//
// 'finish' must not wait for the response either: a server may answer only
// after the client has acted on it ("held until 'finish'").
//
// Only the order of events is printed.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let release = null;
const server = http.createServer((req, res) => {
  if (req.url === "/held") {
    // Answers only once the client says its request has finished.
    release = () => res.end("released");
    return;
  }
  let n = 0;
  req.on("data", (d) => (n += d.length));
  req.on("end", () => res.end(`got ${n}`));
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const P = server.address().port;
// Hangs up as soon as it has read something: a request that left and failed.
const hangUp = net.createServer((s) => {
  s.once("data", () => s.destroy());
  s.on("error", () => {});
});
await new Promise((r) => hangUp.listen(0, "127.0.0.1", r));
const HANG_UP = hangUp.address().port;
// A port nothing listens on.
const closed = net.createServer();
await new Promise((r) => closed.listen(0, "127.0.0.1", r));
const CLOSED = closed.address().port;
await new Promise((r) => closed.close(r));

async function run(label, options, drive) {
  const events = [];
  const cb = (name) => function (err) {
    events.push(`write(${name}) callback${err ? " " + err.code : ""}`);
  };
  const req = http.request({ host: "127.0.0.1", port: P, method: "POST", ...options });
  req.on("socket", () => events.push("socket"));
  req.on("finish", () => {
    events.push(`finish finished=${req.writableFinished}`);
    if (options.path === "/held") setTimeout(() => release(), 20);
  });
  req.on("close", () => events.push("close"));
  await new Promise((resolve) => {
    req.on("response", (res) => {
      events.push("response");
      res.resume();
      res.on("end", () => {
        events.push("end");
        resolve();
      });
    });
    // The callbacks of writes that were never made follow 'close'.
    req.on("error", (e) => {
      events.push(`error ${e.code} finished=${req.writableFinished}`);
      setTimeout(resolve, 100);
    });
    drive(req, events, cb);
  });
  await sleep(30);
  console.log(`${label}: ${events.join(", ")}`);
}

const ended = (req, events, cb) => req.end(cb("end"));
const chunks = (req, events, cb) => {
  req.write("a", cb("a"));
  req.write("b", cb("b"));
  req.end("c", cb("end"));
};
// Each chunk a turn after the last one's callback: a streamed body.
const overTime = (req, events, cb) => {
  const told = cb("a");
  req.write("a", function (err) {
    told.apply(this, arguments);
    setImmediate(() => {
      events.push("later");
      req.write("b", cb("b"));
      req.end("c", cb("end"));
    });
  });
};

for (const [name, drive] of [["ended", ended], ["chunks", chunks]]) {
  await run(`answered, ${name}`, {}, drive);
  await run(`refused, ${name}`, { port: CLOSED }, drive);
  await run(`hung up on, ${name}`, { port: HANG_UP }, drive);
  await run(`held until 'finish', ${name}`, { path: "/held" }, drive);
}
await run("answered, chunks over time", {}, overTime);
await run("held until 'finish', chunks over time", { path: "/held" }, overTime);
await run("refused, a chunk and no end", { port: CLOSED }, (req, events, cb) => {
  req.write("a", cb("a"));
});
await run("answered, GET", { method: "GET" }, ended);
await run("refused, GET", { port: CLOSED, method: "GET" }, ended);
await run("refused, agent: false", { port: CLOSED, agent: false }, chunks);
await run("answered, agent: false", { agent: false }, chunks);
// A second request on the connection the first one left in the pool.
{
  const agent = new http.Agent({ keepAlive: true });
  await run("keep-alive, first", { agent }, chunks);
  await run("keep-alive, second", { agent }, chunks);
  agent.destroy();
}
// Destroyed before anything could leave: no 'finish', no callback.
await run("destroyed at once", {}, (req, events, cb) => {
  req.write("a", cb("a"));
  req.end(cb("end"));
  req.destroy();
});

server.close();
server.closeAllConnections();
hangUp.close();
