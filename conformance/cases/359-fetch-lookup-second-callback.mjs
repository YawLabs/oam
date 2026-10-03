// A lookup hook that calls back more than once (#169). node's
// lookupAndConnectMultiple (lib/net.js) acts on every callback that arrives
// while the socket is still connecting, so after an answer a second callback
// with an error fails the connection -- fetch() rejects with that error as
// its cause -- and so does a second answer node's address rules refuse, or
// the hook throwing once it has answered. With the error first, the first
// error is the cause and the answer after it changes nothing. A callback
// after the socket connected is ignored. oam kept the first callback and
// dropped the rest, so the fetch connected and resolved.
//
// The hook here is a replaced dns.lookup, which node's fetch calls (through
// net.connect) for every connection it opens; an undici Agent's
// connect.lookup goes through the same code. Each case asks for its own host
// name, so each needs a connection of its own and calls the hook. A second
// answer that passes the address rules is left out: node starts a second
// connect on the same socket, which fails with a platform-specific code.
// Measured on node v22.22.2.
import http from "node:http";
import dns from "node:dns";

const server = http.createServer((req, res) => res.end("ok"));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const { port } = server.address();
const ADDR = [{ address: "127.0.0.1", family: 4 }];

const original = dns.lookup;
let hook = null;
dns.lookup = (host, opts, cb) => hook(cb);

async function run(label, behaviour) {
  hook = behaviour;
  try {
    const res = await fetch(`http://${label}.test:${port}/`);
    console.log(label, "resolved", res.status, await res.text());
  } catch (e) {
    const c = e.cause;
    console.log(label, "rejected", e.name, e.message, "| cause", c?.name, c?.message);
  }
}

await run("answer-then-error", (cb) => { cb(null, ADDR); cb(new Error("second")); });
await run("error-then-answer", (cb) => { cb(new Error("first")); cb(null, ADDR); });
await run("answer-then-two-errors", (cb) => { cb(null, ADDR); cb(new Error("second")); cb(new Error("third")); });
await run("answer-then-throw", (cb) => { cb(null, ADDR); throw new Error("thrown"); });
await run("answer-then-empty", (cb) => { cb(null, ADDR); cb(null, []); });
await run("answer-then-bad-ip", (cb) => { cb(null, ADDR); cb(null, [{ address: "nope", family: 4 }]); });
// After the response, the connection is long made: ignored.
let late = null;
await run("error-after-connect", (cb) => { cb(null, ADDR); late = () => cb(new Error("too late")); });
late();
await run("next", (cb) => cb(null, ADDR));

dns.lookup = original;
server.close();
server.closeAllConnections();
