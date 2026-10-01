// Aborting a fetch that has no response head yet takes the request off the
// wire (#158): the server sees the client leave -- `res` 'close' with
// `writableFinished` false -- at the abort, as under node v22.22.2. So does
// `req.destroy()` on an `http.request` that has no response yet.
//
// oam used to reject the fetch promise and leave the request running: the
// server answered a client that was gone, and only then saw the connection
// close. A long poll or a slow endpoint a caller gave up on kept its
// connection, and its work, for as long as the server took.
//
// No timing: the server never answers until the client has left (or a long
// fallback, which prints `answered=true`), and each client aborts when the
// server says the request has arrived.
import http from "node:http";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const arrived = new Map();
const left = new Map();
const server = http.createServer((req, res) => {
  const path = req.url;
  if (path === "/warm") {
    res.end("ok");
    return;
  }
  let answered = false;
  const fallback = setTimeout(() => {
    answered = true;
    res.end("late");
  }, 5000);
  res.on("close", () => {
    clearTimeout(fallback);
    left.get(path)?.(`finished=${res.writableFinished} answered=${answered}`);
  });
  req.on("data", () => {});
  req.on("end", () => arrived.get(path)?.());
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const watch = (path) => ({
  arrived: new Promise((resolve) => arrived.set(path, resolve)),
  left: new Promise((resolve) => left.set(path, resolve)),
});

// A pooled connection, so the aborted requests below go out on one.
await (await fetch(`${base}/warm`)).text();

async function aborted(label, path, init, reason) {
  const seen = watch(path);
  const controller = new AbortController();
  const pending = fetch(`${base}${path}`, { ...init, signal: controller.signal });
  await seen.arrived;
  controller.abort(reason);
  try {
    const res = await pending;
    console.log(`${label}: resolved ${res.status}`);
  } catch (e) {
    console.log(`${label}: rejected ${e.name} ${JSON.stringify(e.message)} cause=${e.cause}`);
  }
  console.log(`${label}: server saw ${await seen.left}`);
}

await aborted("GET", "/get", {});
await aborted("POST", "/post", { method: "POST", body: "x".repeat(4096) });
await aborted("GET with a reason", "/reason", {}, new Error("my reason"));

// The connection the aborted requests used is gone; the next fetch is fine.
console.log(`after the aborts: ${await (await fetch(`${base}/warm`)).text()}`);

{
  const seen = watch("/request");
  const events = [];
  const req = http.request(`${base}/request`, () => events.push("response"));
  req.on("error", (e) => events.push(`error ${e.code} ${e.message}`));
  const closed = new Promise((resolve) => req.on("close", resolve));
  req.end();
  await seen.arrived;
  req.destroy();
  await closed;
  console.log(`http.request destroy: ${events.join(", ")}`);
  console.log(`http.request destroy: server saw ${await seen.left}`);
}

server.close();
server.closeAllConnections();
