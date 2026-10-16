// A pipe server closed while a client it has not yet handed to JS is
// connected (the client saw 'connect'; the server's 'connection' has not
// fired) closes that client's instance too (#388): libuv's uv_close closes
// every instance the server holds, connected or not, before it returns. So
// in the same tick as close() the name is gone (a synchronous open gets
// ENOENT), the name can be listened on again at once, the server never
// emits 'connection' for that client, and the client reads the end of the
// stream.
//
// Regression guard: oam closed only the instances still waiting for a
// client. One whose client had connected stayed open until JS took the
// accept's answer a tick later and reset it, so the open got EBUSY, the
// same-tick listen got EADDRINUSE, and a dial waited in WaitNamedPipeW
// for an instance that never came (ETIMEDOUT after 30 s).
//
// Only a round where the server has not emitted 'connection' by the time
// the client's 'connect' callback runs counts: node always runs the
// client's callback first, oam's server sometimes gets in first, and a
// connection already handed to the server rightly outlives its close.
// Windows only: a Unix socket server has no instances; the expected lines
// are printed there.
import net from "node:net";
import fs from "node:fs";

const pid = String(process.pid);
let serial = 0;
const nextName = () => "\\\\.\\pipe\\oam-case-561-" + pid + "-" + serial++;

const tally = (counts, key) => {
  counts[key] = (counts[key] || 0) + 1;
};

const RUNS = 10;
const MAX_TRIES = 200;

// A server, and a client that has connected to it before the server has
// emitted 'connection' for it; null when the server got in first.
const serverWithClient = async (name) => {
  const round = { connections: 0 };
  const srv = net.createServer((c) => {
    round.connections++;
    c.on("error", () => {});
  });
  await new Promise((resolve, reject) => {
    srv.on("error", reject);
    srv.listen(name, resolve);
  });
  const client = await new Promise((resolve, reject) => {
    const c = net.connect(name, () => resolve(c));
    c.on("error", reject);
  });
  if (round.connections > 0) {
    client.on("error", () => {});
    client.destroy();
    await new Promise((r) => srv.close(r));
    return null;
  }
  return { srv, client, round };
};

// The client's own events after the server closed, once they have settled.
const clientEvents = (client) => {
  const seen = [];
  client.on("error", (e) => seen.push("error " + e.code));
  client.on("end", () => seen.push("end"));
  client.on("close", () => seen.push("close"));
  client.resume();
  return seen;
};

const settle = () => new Promise((r) => setTimeout(r, 100));

// `body` for RUNS rounds that count.
const rounds = async (body) => {
  let done = 0;
  for (let tries = 0; done < RUNS && tries < MAX_TRIES; tries++) {
    const round = await serverWithClient(nextName());
    if (round === null) continue;
    await body(round);
    done++;
  }
  if (done < RUNS) console.log("only", done, "rounds counted");
};

if (process.platform === "win32") {
  {
    const counts = {};
    const events = {};
    await rounds(async ({ srv, client, round }) => {
      const seen = clientEvents(client);
      srv.close();
      try {
        fs.closeSync(fs.openSync(srv.address(), "r"));
        tally(counts, "opened");
      } catch (e) {
        tally(counts, e.code);
      }
      await settle();
      tally(events, seen.join(","));
      if (round.connections > 0) tally(counts, "connection after close");
      client.destroy();
    });
    console.log("open right after close", JSON.stringify(counts));
    console.log("client events", JSON.stringify(events));
  }
  {
    const counts = {};
    await rounds(async ({ srv, client, round }) => {
      client.on("error", () => {});
      srv.close();
      const again = net.createServer();
      const outcome = await new Promise((resolve) => {
        again.on("error", (e) => resolve(e.code));
        again.listen(srv.address(), () => resolve("listening"));
      });
      tally(counts, outcome);
      await settle();
      if (round.connections > 0) tally(counts, "connection after close");
      client.destroy();
      if (again.listening) await new Promise((r) => again.close(r));
    });
    console.log("listen again right after close", JSON.stringify(counts));
  }
} else {
  console.log("open right after close", JSON.stringify({ ENOENT: RUNS }));
  console.log("client events", JSON.stringify({ "end,close": RUNS }));
  console.log("listen again right after close", JSON.stringify({ listening: RUNS }));
}
