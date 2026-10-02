// A request whose response has already finished, and whose client then
// goes away -- or resets -- while still uploading: node's server has taken
// it off the connection's incoming queue (resOnFinish), so the
// connection's close aborts nothing -- no 'aborted', no 'error' on the
// request -- while its socket reports the failure (HPE_INVALID_EOF_STATE or
// read ECONNRESET) and closes with true (measured on node v22.22.2). oam
// closed the socket with false and, when the body's failure came in after
// the close, emitted 'aborted' and ECONNRESET "aborted" on the request.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const wait = (ms) => new Promise((r) => setTimeout(r, ms));

for (const [label, leaveLater, onAnswer] of [
  ["the client goes away", (c) => c.destroy(), null],
  ["the client resets", (c) => c.resetAndDestroy(), null],
  ["the client goes away as the answer arrives", null, (c) => c.destroy()],
  ["the client resets as the answer arrives", null, (c) => c.resetAndDestroy()],
]) {
  const log = [];
  let socketClosed;
  const closed = new Promise((r) => { socketClosed = r; });
  const server = http.createServer((req, res) => {
    let n = 0;
    req.on("data", (c) => { n += c.length; });
    req.on("aborted", () => log.push(`request aborted after ${n}`));
    req.on("error", (e) => log.push(`request error ${e.code} ${e.message}`));
    req.on("end", () => log.push(`request end ${n}`));
    res.on("finish", () => log.push("response finish"));
    req.socket.on("error", (e) => log.push(`socket error ${e.code}`));
    req.socket.on("close", (hadError) => {
      log.push(`socket close ${hadError}, got ${n}`);
      socketClosed();
    });
    res.end("early");
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const client = net.connect(server.address().port, "127.0.0.1");
  client.on("error", () => {});
  client.on("data", () => {
    if (onAnswer) onAnswer(client);
  });
  client.write("POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n");
  if (leaveLater) {
    await wait(150);
    client.write("3\r\ndef\r\n");
    await wait(150);
    leaveLater(client);
  }
  await closed;
  await wait(200);
  server.close();
  console.log(`${label}: ${log.join(", ")}`);
}
