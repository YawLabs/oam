// A Blob as util.inspect shows it (node's custom inspect: `Blob { size,
// type }`, a File with its name and lastModified, nothing of the internals,
// not even a slice's own `constructor`), and fs.openAsBlob's Blob sent as a
// fetch body (#373): on its own, in a FormData and in a Blob of mixed parts
// it goes out with its content-length, read when the request is sent.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import http from "node:http";
import util from "node:util";

const b = new Blob(["hello"], { type: "a/b" });
console.log(util.inspect(b), "|", util.inspect(b.slice(1)), "|", String(b), JSON.stringify(b), JSON.stringify({ b }));
console.log(util.inspect({ a: b }, { depth: 0 }), util.inspect([[b]], { depth: 1 }), util.inspect([b], { depth: 1 }));
console.log(util.inspect(new File(["x"], "n.txt", { type: "t/p", lastModified: 42 })));
console.log(util.inspect({ f: new File(["x"], "n.txt", { lastModified: 1 }) }, { depth: 0 }));
console.log(JSON.stringify(util.inspect({ a: b }, { depth: 0, colors: true })));

const D = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-524-"));
const f = path.join(D, "f.txt");
fs.writeFileSync(f, "file-body");
const fb = await fs.openAsBlob(f, { type: "text/plain" });
console.log(util.inspect(fb), util.inspect(fb.slice(0, 4)));

const server = http.createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () =>
    res.end(
      JSON.stringify({
        length: req.headers["content-length"],
        chunked: req.headers["transfer-encoding"] === "chunked",
        type: (req.headers["content-type"] ?? "").replace(/boundary=.*/, "boundary=X"),
        body: body.replace(/-+formdata-[a-z0-9-]+/g, "--B"),
      }),
    ),
  );
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const url = `http://127.0.0.1:${server.address().port}/`;
const post = async (body) => (await fetch(url, { method: "POST", body })).text();
console.log("a file-backed Blob:", await post(fb));
const form = new FormData();
form.append("f", fb, "f.txt");
form.append("s", "v");
const sent = JSON.parse(await post(form));
console.log("in a FormData: has a length", Number(sent.length) > 200, "chunked", sent.chunked, sent.type, JSON.stringify(sent.body));
console.log("in a Blob of mixed parts:", await post(new Blob(["mem:", fb])));
fs.writeFileSync(f, "changed!!");
const failed = await fetch(url, { method: "POST", body: fb }).then(
  () => "sent",
  (e) => `${e.name}: ${e.message} (cause ${e.cause?.name})`,
);
console.log("after the file changed:", failed);
server.close();
console.log("Response(fb).blob():", await new Response(await fs.openAsBlob(f)).blob().then((x) => x.text()));
fs.rmSync(D, { recursive: true, force: true });
