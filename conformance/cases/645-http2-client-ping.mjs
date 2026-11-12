// ClientHttp2Session.ping(payload?, callback): a PING frame goes to the
// server and the callback comes at its ACK with (null, duration, payload) --
// the duration a finite number of milliseconds, the payload a Buffer of its
// own holding the eight bytes the ACK echoed (the caller's, whatever view
// they came in; the session's own choice when there were none). Pinned with
// it: the argument checks and their order (a destroyed session, then the
// payload's type, its length in bytes, the callback); the ping that cannot
// be answered -- made while connecting, after close(), over
// maxOutstandingPings (returns false, its callback called before ping()
// returns) or still waiting when the session is destroyed -- which gets
// ERR_HTTP2_PING_CANCEL as its only argument; pings answered in the order
// sent; and no 'ping' event on the session for its own ACK.
//
// Regression guard: oam's ping() sent nothing and called back at once with
// a duration of 0 and the caller's own buffer (zeros when there was none),
// checked no argument, and "succeeded" on a session that was connecting,
// closed or destroyed -- so a keep-alive built on it never noticed a dead
// connection.
//
// The duration's value is the one thing not printed.
import http2 from "node:http2";

const server = http2.createServer();
let release = () => {};
server.on("stream", (stream, headers) => {
  stream.respond({ ":status": 200 });
  // "/held" ends when the client says so.
  if (headers[":path"] === "/held") release = () => stream.end("held");
  else stream.end("ok");
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const origin = `http://127.0.0.1:${server.address().port}`;

const attempt = (label, fn) => {
  try {
    console.log(label, "returned", fn());
  } catch (e) {
    console.log(label, "threw", e.name, e.code, JSON.stringify(e.message));
  }
};
const tick = () => new Promise((resolve) => setImmediate(resolve));
const noop = () => {};

const client = http2.connect(origin);
let pingEvents = 0;
client.on("ping", () => pingEvents++);

// Connecting: the arguments are checked, and a good ping is cancelled.
console.log("connecting", client.connecting);
attempt("connecting", () =>
  client.ping(function (...args) {
    console.log("connecting callback", args.length, args[0].name, args[0].code, JSON.stringify(args[0].message));
  }));
attempt("no callback", () => client.ping());
attempt("payload, no callback", () => client.ping(Buffer.alloc(8)));
attempt("callback not a function", () => client.ping(Buffer.alloc(8), "x"));
attempt("string payload", () => client.ping("12345678", noop));
attempt("number payload", () => client.ping(5, noop));
attempt("object payload", () => client.ping({}, noop));
attempt("ArrayBuffer payload", () => client.ping(new ArrayBuffer(8), noop));
attempt("7 bytes", () => client.ping(Buffer.alloc(7), noop));
attempt("9 bytes", () => client.ping(new Uint8Array(9), noop));
attempt("8 elements of 2 bytes", () => client.ping(new Uint16Array(8), noop));
attempt("length before callback", () => client.ping(Buffer.alloc(7)));
// A falsy payload is no payload.
attempt("null payload", () => client.ping(null, noop));
attempt("0 payload", () => client.ping(0, noop));
attempt("empty string payload", () => client.ping("", noop));
await new Promise((resolve) => client.once("connect", resolve));
await tick();

// Connected: the ACK answers.
const ping = (label, ...payload) =>
  new Promise((resolve) => {
    let sync = true;
    attempt(label, () =>
      client.ping(...payload, function (...args) {
        const [err, duration, echoed] = args;
        console.log(
          label, "callback", args.length, err, typeof duration,
          Number.isFinite(duration) && duration >= 0,
          Buffer.isBuffer(echoed), echoed.length,
          payload.length > 0 ? echoed.toString("hex") : "(the session's)",
          this === undefined, sync ? "sync" : "async",
        );
        resolve(echoed);
      }));
    sync = false;
  });
const mine = Buffer.from("abcdefgh");
console.log("the caller's buffer", (await ping("Buffer", mine)) === mine);
await ping("no payload");
await ping("Uint8Array", new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]));
await ping("Uint16Array", new Uint16Array([0x0201, 0x0403, 0x0605, 0x0807]));
await ping("DataView", new DataView(Uint8Array.of(9, 8, 7, 6, 5, 4, 3, 2).buffer));
await ping("a view into a larger buffer", Buffer.from("xxabcdefghxx").subarray(2, 10));
// The bytes are taken at the call.
const reused = Buffer.from("12345678");
const sent = ping("reused buffer", reused);
reused.fill(0);
await sent;

// Several at once are answered in the order sent.
const order = [];
await Promise.all([1, 2, 3].map((n) =>
  new Promise((resolve) => {
    const returned = client.ping(Buffer.alloc(8, n), (err, duration, echoed) => {
      order.push(`${err}:${echoed[0]}`);
      resolve();
    });
    order.push(`sent ${n} ${returned}`);
  })));
console.log("concurrent", order.join(" "));

// maxOutstandingPings (10): the eleventh is refused, before ping() returns.
const outcomes = [];
const returns = [];
const waits = [];
for (let n = 0; n < 12; n++) {
  waits.push(new Promise((resolve) => {
    returns.push(client.ping((err) => {
      outcomes.push(`${n}:${err ? err.code : "ack"}`);
      resolve();
    }));
  }));
}
console.log("over the limit, returned", returns.join(" "));
console.log("over the limit, before returning", outcomes.join(" "));
await Promise.all(waits);
console.log("over the limit, all", outcomes.join(" "));

// A stream still works after all that, and the session heard no 'ping'.
const request = client.request({ ":path": "/" });
let body = "";
request.setEncoding("utf8");
request.on("data", (chunk) => (body += chunk));
await new Promise((resolve) => request.on("close", resolve));
console.log("a request after the pings", body, "'ping' events", pingEvents);

// Closed, a stream still open: cancelled, the arguments still checked.
const held = client.request({ ":path": "/held" });
held.resume();
await new Promise((resolve) => held.once("response", resolve));
client.close();
console.log("closed", client.closed, "destroyed", client.destroyed);
attempt("closed", () =>
  client.ping(function (...args) {
    console.log("closed callback", args.length, args[0].code);
  }));
attempt("closed, 3 bytes", () => client.ping(Buffer.alloc(3), noop));
await tick();
release();
await new Promise((resolve) => client.once("close", resolve));
await tick();

// Destroyed: refused before anything else is looked at.
console.log("destroyed", client.destroyed);
attempt("destroyed", () => client.ping(noop));
attempt("destroyed, no callback", () => client.ping());
attempt("destroyed, bad payload", () => client.ping("x"));

// Destroyed with a ping waiting: cancelled, ahead of 'close'.
const second = http2.connect(origin);
await new Promise((resolve) => second.once("connect", resolve));
const events = [];
attempt("second", () =>
  second.ping(function (...args) {
    events.push(`callback ${args.length} ${args[0].code}`);
  }));
second.on("close", () => events.push("close"));
second.destroy();
await new Promise((resolve) => second.once("close", resolve));
await tick();
console.log("destroyed while waiting:", events.join(", "));

// maxOutstandingPings: 1.
const third = http2.connect(origin, { maxOutstandingPings: 1 });
await new Promise((resolve) => third.once("connect", resolve));
const thirdEvents = [];
const first = new Promise((resolve) => {
  thirdEvents.push(third.ping((err) => { thirdEvents.push(`first ${err}`); resolve(); }));
});
thirdEvents.push(third.ping((err) => thirdEvents.push(`second ${err.code}`)));
await first;
console.log("maxOutstandingPings 1:", thirdEvents.join(", "));
third.close();
await new Promise((resolve) => third.once("close", resolve));
server.close();
await new Promise((resolve) => server.once("close", resolve));
console.log("done");
