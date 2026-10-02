// fetch() with a streamed body is dispatched at once, as undici dispatches
// it: a bad port, a failed lookup or a refused connect fail the fetch while
// the source has produced nothing, and a source whose request never got a
// connection is never read. The head waits for the first non-empty chunk:
// a server sees nothing of the request until then. oam held the dispatch
// until the first chunk, so a fetch whose source was idle hung, and its
// source was read before the failure. Measured on node v22.22.2.
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const describe = (e) =>
  `${e.name}: ${e.message}` + (e.cause ? ` | cause ${e.cause.code ?? ""} ${e.cause.message}` : "");

// A source that counts its pulls and never produces. A high-water mark of
// 0 keeps the stream from pulling on its own, so a pull is a read.
function idle() {
  const counter = { pulls: 0 };
  const stream = new ReadableStream({
    pull() {
      counter.pulls++;
      return new Promise(() => {});
    },
  }, { highWaterMark: 0 });
  return { stream, counter };
}

const closed = net.createServer();
await new Promise((r) => closed.listen(0, "127.0.0.1", r));
const deadPort = closed.address().port;
await new Promise((r) => closed.close(r));

for (const [label, url] of [
  ["bad port", "http://127.0.0.1:25/"],
  ["refused", `http://127.0.0.1:${deadPort}/`],
  ["no such host", "http://nohost.invalid/"],
]) {
  const { stream, counter } = idle();
  const outcome = await Promise.race([
    fetch(url, { method: "POST", body: stream, duplex: "half" }).then(
      (r) => `status ${r.status}`,
      (e) => describe(e).replace(/:\d+$/, ":<port>").replace(/127\.0\.0\.1:\d+/, "127.0.0.1:<port>"),
    ),
    sleep(5000).then(() => "still pending after 5 s"),
  ]);
  console.log(`${label}: ${outcome}; source pulled ${counter.pulls > 0 ? "yes" : "no"}`);
}

// The head waits for the first chunk; the connection is there before it.
{
  const events = [];
  const server = net.createServer((socket) => {
    events.push("connection");
    let seen = "";
    socket.on("data", (d) => {
      if (seen === "") events.push("first bytes");
      seen += d.toString("latin1");
      if (seen.endsWith("0\r\n\r\n")) {
        events.push(`head ${seen.split("\r\n")[0]}, chunked ${/transfer-encoding: chunked/i.test(seen)}`);
        socket.end("HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
      }
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  let release;
  const gate = new Promise((r) => { release = r; });
  const body = new ReadableStream({
    async pull(controller) {
      await gate;
      controller.enqueue(new TextEncoder().encode("late"));
      controller.close();
    },
  });
  const pending = fetch(`http://127.0.0.1:${server.address().port}/up`, {
    method: "POST",
    body,
    duplex: "half",
  });
  await sleep(300);
  events.push(`before the chunk: ${events.length ? events.join(", ") : "nothing"}`);
  release();
  const res = await pending;
  events.push(`response ${res.status} ${await res.text()}`);
  server.close();
  for (const e of events) console.log(e);
}
