// What a `fetch`, a `Request` and a `Response` make of each kind of body
// (#154): the Fetch Standard's "extract a body". Measured on node v22.22.2
// first. oam used to handle a string and bytes and pass everything else
// through `String()`, so a Blob went out as the 13 bytes `[object Blob]`, a
// FormData as `[object FormData]`, a ReadableStream as `[object
// ReadableStream]`, a URLSearchParams with `text/plain`, and
// `new Response(blob).text()` threw.
//
//   - Blob / File: its bytes, and its `type` as the content-type when it has
//     one.
//   - URLSearchParams: its serialisation, as
//     `application/x-www-form-urlencoded;charset=UTF-8`.
//   - FormData: a multipart/form-data body. The boundary is each runtime's
//     own, so it is replaced before printing and the length is checked
//     against the body rather than printed.
//   - ReadableStream (and any async iterable): sent chunked, and refused
//     without `duplex: 'half'`.
//   - ArrayBuffer, typed arrays, DataView: the bytes, no content-type.
//   - Anything else: `String(body)` as text/plain.
//   - A caller's own content-type wins over all of them.
//
// Not asserted: `formData()`, which oam's Request and Response do not have
// (docs/node-divergences.md), and a `GET` with a body, which belongs to the
// `fetch(Request)` case.
import http from "node:http";

const srv = http.createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    const body = Buffer.concat(chunks);
    res.end(
      JSON.stringify({
        type: req.headers["content-type"] ?? null,
        framing:
          req.headers["transfer-encoding"] !== undefined
            ? "transfer-encoding " + req.headers["transfer-encoding"]
            : req.headers["content-length"] === String(body.length)
              ? "content-length matches"
              : "content-length " + req.headers["content-length"],
        body: body.toString("latin1"),
      }),
    );
  });
});
await new Promise((resolve) => srv.listen(0, "127.0.0.1", resolve));
const U = `http://127.0.0.1:${srv.address().port}/p`;

// The boundary differs per runtime (and per call): keep its shape, drop it.
const stable = (text) => String(text).replace(/----formdata-[a-z]+-\d+/g, "----BOUNDARY");

const form = () => {
  const f = new FormData();
  f.append("k", "v");
  f.append("f", new Blob(["file\r\ndata"], { type: "text/x-t" }), 'a"b\n.txt');
  f.append("g", new File(["x"], "g.bin"));
  f.append("b", new Blob(["z"]));
  f.append('na"me\r\n', "line1\nline2\rline3\r\nend");
  f.append("é", "ü");
  return f;
};
const stream = (...chunks) =>
  new ReadableStream({
    start(c) {
      for (const chunk of chunks) c.enqueue(chunk);
      c.close();
    },
  });
const utf8 = (text) => new TextEncoder().encode(text);

async function post(label, body, extra) {
  let line;
  try {
    const r = await fetch(U, { method: "POST", body: body(), ...extra });
    line = await r.text();
  } catch (e) {
    line = `reject ${e.constructor.name}: ${e.message}`;
  }
  console.log("fetch " + label.padEnd(22) + " " + stable(line));
}

await post("string", () => "plain");
await post("Uint8Array", () => new Uint8Array([104, 105]));
await post("ArrayBuffer", () => new Uint8Array([104, 105]).buffer);
await post("DataView", () => new DataView(new Uint8Array([104, 105]).buffer));
await post("Blob with type", () => new Blob(["blobdata"], { type: "application/x-test" }));
await post("Blob no type", () => new Blob(["blobdata"]));
await post("File", () => new File(["filedata"], "f.txt", { type: "text/x-file" }));
await post("Blob, caller type", () => new Blob(["blobdata"], { type: "application/x-test" }), {
  headers: { "content-type": "x/y" },
});
await post("URLSearchParams", () => new URLSearchParams({ a: "1", b: "x y" }));
await post("FormData", form);
await post("stream, duplex half", () => stream(utf8("stre"), utf8("amed")), { duplex: "half" });
await post("stream, string chunk", () => stream("str"), { duplex: "half" });
await post("stream, no duplex", () => stream(utf8("streamed")));
await post(
  "stream, locked",
  () => {
    const s = stream(utf8("x"));
    s.getReader();
    return s;
  },
  { duplex: "half" },
);
await post(
  "async iterable",
  () =>
    (async function* () {
      yield utf8("it");
      yield utf8("er");
    })(),
  { duplex: "half" },
);
await post("object", () => ({ a: 1 }));
await post("number", () => 42);

// The same extraction behind the constructors: the content-type it implies,
// the body read back, and a body read once.
async function built(label, make) {
  let line;
  try {
    const x = make();
    const before = [x.headers.get("content-type"), x.bodyUsed, x.body === null ? null : x.body.constructor.name];
    const text = await x.text();
    let second;
    try {
      await x.text();
      second = "read again";
    } catch (e) {
      second = `${e.constructor.name}: ${e.message}`;
    }
    line = JSON.stringify([...before, text, x.bodyUsed, second]);
  } catch (e) {
    line = `throws ${e.constructor.name}: ${e.message}`;
  }
  console.log(label.padEnd(28) + " " + stable(line));
}

await built("Response string", () => new Response("x"));
await built("Response null", () => new Response(null));
await built("Response none", () => new Response());
await built("Response Blob", () => new Response(new Blob(["blobdata"], { type: "application/x-test" })));
await built("Response Blob no type", () => new Response(new Blob(["blobdata"])));
await built("Response URLSearchParams", () => new Response(new URLSearchParams({ a: "1" })));
await built("Response FormData", () => new Response(form()));
await built("Response Uint8Array", () => new Response(new Uint8Array([104, 105])));
await built("Response stream", () => new Response(stream(utf8("s"))));
await built("Response stream, string", () => new Response(stream("s")));
await built("Response caller type", () =>
  new Response(new Blob(["b"], { type: "a/b" }), { headers: { "content-type": "c/d" } }));
await built("Response object", () => new Response({ a: 1 }));
await built("Request string", () => new Request(U, { method: "POST", body: "x" }));
await built("Request Blob", () =>
  new Request(U, { method: "POST", body: new Blob(["blobdata"], { type: "application/x-test" }) }));
await built("Request URLSearchParams", () =>
  new Request(U, { method: "POST", body: new URLSearchParams({ a: "1" }) }));
await built("Request FormData", () => new Request(U, { method: "POST", body: form() }));
await built("Request stream, half", () =>
  new Request(U, { method: "POST", body: stream(utf8("s")), duplex: "half" }));
await built("Request stream, no duplex", () => new Request(U, { method: "POST", body: stream(utf8("s")) }));
await built("Request none", () => new Request(U));

// The other readers, clone(), and what counts as used.
{
  const blob = await new Response(new Blob(["blobdata"], { type: "application/x-test" })).blob();
  console.log("blob()", blob.size, blob.type);
  console.log("arrayBuffer()", new Uint8Array(await new Response("hi").arrayBuffer()).join(","));
  const bytes = await new Response("hi").bytes();
  console.log("bytes()", bytes.constructor.name, bytes.join(","));
  console.log("json()", JSON.stringify(await new Response('{"a":1}').json()));
  const sub = new Uint8Array([1, 2, 3, 4]).subarray(1, 3);
  console.log("arrayBuffer() of a subarray", (await new Response(sub).arrayBuffer()).byteLength);

  // The constructor copies a caller's buffer.
  const source = new Uint8Array([65, 66]);
  const kept = new Response(source);
  source[0] = 67;
  console.log("buffer copied", await kept.text());

  const original = new Response("cl", { status: 201, headers: { "x-a": "1" } });
  const copy = original.clone();
  console.log("clone", await original.text(), await copy.text(), copy.status, copy.headers.get("x-a"));
  const streamed = new Response(stream(utf8("A")));
  const streamedCopy = streamed.clone();
  console.log("clone of a stream", await streamed.text(), await streamedCopy.text());
  const request = new Request(U, { method: "POST", body: "rq", headers: { "x-a": "1" } });
  const requestCopy = request.clone();
  console.log("Request clone", await request.text(), await requestCopy.text(), requestCopy.method);
  try {
    await original.clone();
  } catch (e) {
    console.log("clone when used:", e.constructor.name, e.message);
  }

  // `bodyUsed` follows the stream: asking for `.body` does not use it,
  // reading from it does, and a locked body cannot be read through text().
  const viaStream = new Response("x");
  const reader = viaStream.body.getReader();
  console.log("locked, not read", viaStream.bodyUsed);
  try {
    await viaStream.text();
  } catch (e) {
    console.log("text() while locked:", e.constructor.name, e.message);
  }
  await reader.read();
  console.log("after a read", viaStream.bodyUsed);
  console.log("body is one stream", viaStream.body === viaStream.body);
  console.log("'bodyUsed' in prototype", "bodyUsed" in Response.prototype, "bodyUsed" in Request.prototype);
  console.log("own keys", JSON.stringify(Object.keys(new Request(U)).includes("bodyUsed")));

  // A Request built from a Request takes its body.
  const first = new Request(U, { method: "POST", body: "moved", headers: { a: "1" } });
  const second = new Request(first);
  console.log("moved", first.bodyUsed, await second.text(), second.headers.get("content-type"));
  try {
    new Request(first);
  } catch (e) {
    console.log("from a used Request:", e.constructor.name, e.message);
  }
  const replaced = new Request(new Request(U, { method: "POST", body: "old" }), { body: "new" });
  console.log("init body wins", await replaced.text());

  const json = Response.json({ a: 1 });
  console.log("Response.json", json.headers.get("content-type"), await json.text());

  // FormData turns a Blob into a File.
  const f = new FormData();
  f.append("b", new Blob(["z"]));
  f.append("c", new File(["q"], "orig.txt", { type: "text/plain" }), "over.txt");
  console.log("FormData blob", f.get("b").constructor.name, f.get("b").name, f.get("c").name, f.get("c").type);
}

srv.close();
