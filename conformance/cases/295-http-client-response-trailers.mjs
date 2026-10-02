// An http.request's response has node's `trailers` and `rawTrailers` from
// the start -- {} and [] -- and a chunked body's trailer section fills them
// before 'end', on oam's own transport and over an agent's socket alike
// (measured on node v22.22.2). oam's client response had neither, so it
// could not read the trailers oam's own server sends (case 268).
import http from "node:http";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const server = http.createServer((req, res) => {
  if (req.url === "/trailers") {
    res.setHeader("Trailer", "x-t, x-u");
    res.write("a");
    res.addTrailers({ "x-t": "v", "x-u": "w" });
    res.end();
  } else {
    res.end("x");
  }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
for (const path of ["/", "/trailers"]) {
  for (const [label, agent] of [["own transport", undefined], ["agent", new http.Agent()], ["agent: false", false]]) {
    await new Promise((resolve) =>
      http.get({ host: "127.0.0.1", port, path, agent }, (res) => {
        const before = `${JSON.stringify(res.rawTrailers)} ${JSON.stringify(res.trailers)}`;
        res.resume();
        res.on("end", () => {
          console.log(`${path} (${label}): at the head ${before}, at the end ${JSON.stringify(res.rawTrailers)} ${JSON.stringify(res.trailers)}`);
          resolve();
        });
      }));
  }
}
server.close();
