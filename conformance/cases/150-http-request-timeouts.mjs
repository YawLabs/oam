// A ClientRequest's idle timeout, as node's (measured on node v22.22.2):
// the `timeout` option, req.setTimeout(ms[, cb]) and an agent's `timeout`
// fire 'timeout' on the request once its socket has been idle that long --
// before the response head, or while its body stalls -- and a response still
// being read hears it too; activity (the head, each body chunk, each upload
// chunk) re-arms it, and nothing fires once the response has ended or the
// request has failed. oam used to fire none of them for a request on its own
// transport (only one sent over an agent's socket timed out), and for a while
// fired one after a failed request's error. Destroying the request before its
// response -- the usual reply to 'timeout' -- fails it with node's
// ECONNRESET 'socket hang up'; abort() too; destroy(err) with that error.
//
// Only the order of events is printed (and whether the response closed):
// every server delay is several times the timeout it is measured against.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const server = http.createServer((req, res) => {
  const u = new URL(req.url, "http://x");
  const head = Number(u.searchParams.get("head") || 0);
  const body = Number(u.searchParams.get("body") || 0);
  const trickle = Number(u.searchParams.get("trickle") || 0);
  req.resume();
  req.on("end", () => {
    setTimeout(() => {
      if (trickle) {
        res.writeHead(200, { "content-length": String(trickle) });
        let sent = 0;
        const tick = setInterval(() => {
          res.write("x");
          if (++sent === trickle) {
            clearInterval(tick);
            res.end();
          }
        }, 60);
        return;
      }
      res.writeHead(200, { "content-length": "4" });
      res.write("ab");
      setTimeout(() => res.end("cd"), body);
    }, head);
  });
});
server.keepAliveTimeout = 60000;
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

// A subclass whose createConnection is the stock one: the request goes over
// the socket it returns (in oam: the agent path).
class WrappingAgent extends http.Agent {
  createConnection(options, cb) {
    return super.createConnection(options, cb);
  }
}

async function run(label, path, setup, extra = {}) {
  const events = [];
  let resClosed = null;
  await new Promise((resolve) => {
    let req;
    try {
      req = http.request({ host: "127.0.0.1", port, path, agent: new http.Agent(), ...extra });
    } catch (e) {
      events.push(`THROW ${e.code}`);
      resolve();
      return;
    }
    req.on("response", (res) => {
      events.push("response");
      res.on("data", () => {});
      res.on("timeout", () => events.push("res timeout"));
      res.on("end", () => events.push("res end"));
      res.on("aborted", () => events.push("res aborted"));
      res.on("error", (e) => events.push(`res error ${e.code} ${e.message}`));
      resClosed = false;
      res.on("close", () => (resClosed = true));
    });
    req.on("error", (e) => events.push(`error ${e.code} ${e.message}`));
    req.on("abort", () => events.push("abort"));
    req.on("close", () => {
      events.push("close");
      setTimeout(resolve, 80);
    });
    setup(req, events);
    if (!req.writableEnded && !extra.noEnd) req.end();
  });
  console.log(`${label}: ${JSON.stringify(events)}${resClosed === null ? "" : ` res closed=${resClosed}`}`);
}

const destroyOnTimeout = (req, events) =>
  req.on("timeout", () => {
    events.push("timeout");
    req.destroy();
  });
const recordTimeout = (req, events) => req.on("timeout", () => events.push("timeout"));

for (const [kind, agent] of [["transport", () => new http.Agent()], ["agent socket", () => new WrappingAgent()]]) {
  const a = (extra = {}) => ({ agent: agent(), ...extra });
  await run(`${kind}: timeout option, late head, destroy`, "/?head=700", destroyOnTimeout, a({ timeout: 150 }));
  await run(`${kind}: setTimeout, late head, destroy`, "/?head=700", (req, e) => {
    destroyOnTimeout(req, e);
    req.setTimeout(150);
  }, a());
  await run(`${kind}: setTimeout callback, late head`, "/?head=700", (req, e) => req.setTimeout(150, () => e.push("callback")), a());
  await run(`${kind}: timeout option, late body, destroy`, "/?body=700", destroyOnTimeout, a({ timeout: 150 }));
  await run(`${kind}: timeout option, late body`, "/?body=700", recordTimeout, a({ timeout: 150 }));
  await run(`${kind}: timeout option, prompt answer`, "/", recordTimeout, a({ timeout: 150 }));
  await run(`${kind}: body trickling faster than the timeout`, "/?trickle=8", recordTimeout, a({ timeout: 300 }));
  await run(`${kind}: setTimeout after the head`, "/?body=700", (req, e) => {
    recordTimeout(req, e);
    req.on("response", () => req.setTimeout(150));
  }, a());
  await run(`${kind}: setTimeout after the end`, "/", (req, e) => {
    recordTimeout(req, e);
    req.on("response", (res) => res.on("end", () => req.setTimeout(20)));
  }, a());
  await run(`${kind}: the agent's timeout, late head`, "/?head=700", recordTimeout, {
    agent: kind === "transport" ? new http.Agent({ timeout: 150 }) : new WrappingAgent({ timeout: 150 }),
  });
  await run(`${kind}: upload trickling faster than the timeout`, "/", (req, e) => {
    recordTimeout(req, e);
    let n = 0;
    const tick = setInterval(() => {
      req.write("u");
      if (++n === 6) {
        clearInterval(tick);
        req.end();
      }
    }, 60);
  }, a({ method: "POST", timeout: 300, noEnd: true }));
}

// A request that fails hears no timeout after its error: node's socket is
// destroyed with it.
const rude = net.createServer((c) => c.destroy());
await new Promise((r) => rude.listen(0, "127.0.0.1", r));
const gone = net.createServer();
await new Promise((r) => gone.listen(0, "127.0.0.1", r));
const gonePort = gone.address().port;
await new Promise((r) => gone.close(r));
for (const [kind, agent] of [["transport", () => new http.Agent()], ["agent socket", () => new WrappingAgent()]]) {
  for (const [what, to] of [["hung up", rude.address().port], ["refused", gonePort]]) {
    const events = [];
    await new Promise((resolve) => {
      const req = http.get({ host: "127.0.0.1", port: to, agent: agent(), timeout: 100 });
      req.on("timeout", () => events.push("timeout"));
      req.on("error", (e) => events.push(`error ${e.code}`));
      req.on("close", () => {
        events.push("close");
        setTimeout(resolve, 400);
      });
    });
    console.log(`${kind}: ${what}, timeout option: ${JSON.stringify(events)}`);
  }
}
rude.close();

await run("destroy() at once", "/?head=300", (r) => r.destroy());
await run("destroy() from 'socket'", "/?head=300", (r) => r.on("socket", () => r.destroy()));
await run("destroy() before the head", "/?head=300", (r) => setTimeout(() => r.destroy(), 100));
await run("abort() before the head", "/?head=300", (r) => setTimeout(() => r.abort(), 100));
await run("destroy(err) before the head", "/?head=300", (r) =>
  setTimeout(() => r.destroy(Object.assign(new Error("mine"), { code: "EMINE" })), 100));
await run("destroy() in the body", "/?body=300", (r) => r.on("response", () => setTimeout(() => r.destroy(), 50)));
await run("abort() in the body", "/?body=300", (r) => r.on("response", () => setTimeout(() => r.abort(), 50)));
await run("agent socket: destroy() before the head", "/?head=300", (r) => setTimeout(() => r.destroy(), 100), {
  agent: new WrappingAgent(),
});
await run("timeout option a string", "/", () => {}, { timeout: "x" });
await run("timeout option negative", "/", () => {}, { timeout: -1 });
await run("setTimeout(-1)", "/", (r, e) => {
  try {
    r.setTimeout(-1);
  } catch (err) {
    e.push(`setTimeout(-1) ${err.code}`);
  }
});
await run("setTimeout('1')", "/", (r, e) => {
  try {
    r.setTimeout("1");
  } catch (err) {
    e.push(`setTimeout('1') ${err.code}`);
  }
});
console.log("global agent timeout:", http.globalAgent.options.timeout);
server.close();
