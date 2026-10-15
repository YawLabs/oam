// v8.getHeapSnapshot() is node's HeapSnapshotStream: a Readable of Buffers
// whose bytes are one .heapsnapshot JSON document with V8's shape (snapshot
// meta, nodes/edges/strings arrays whose lengths agree with that meta), the
// kind Chrome DevTools loads. v8.writeHeapSnapshot() writes the same document
// and returns the filename as validated -- the string, the very Buffer, a
// file: URL's path -- or, with none, node's generated
// Heap.<date>.<time>.<pid>.<threadId>.<seq>.heapsnapshot in the cwd, its seq
// one process-wide counter a worker's file continues. Bad arguments throw
// node's errors; worker.getHeapSnapshot() on an exited worker rejects with
// ERR_WORKER_NOT_RUNNING.
//
// Regression guard: oam had no getHeapSnapshot export at all (a link-time
// SyntaxError for any import of it), and writeHeapSnapshot wrote nothing and
// returned "".
//
// Only deterministic properties are printed: the snapshot's contents differ
// run to run and between V8 versions, so the case checks shape and internal
// consistency -- indexing nodes by the field layout `meta` declares, as
// DevTools does -- never counts or bytes.
import v8, { getHeapSnapshot, writeHeapSnapshot } from "node:v8";
import { Readable } from "node:stream";
import { Worker } from "node:worker_threads";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-heapsnap-"));
const home = process.cwd();
process.chdir(dir);

function checkShape(label, text) {
  const j = JSON.parse(text);
  const s = j.snapshot;
  const m = s.meta;
  const nf = m.node_fields.length;
  const ef = m.edge_fields.length;
  const edgesFromNodes = j.nodes.reduce(
    (sum, _, i) => (i % nf === m.node_fields.indexOf("edge_count") ? sum + j.nodes[i] : sum),
    0,
  );
  console.log(label, JSON.stringify({
    topKeys: Object.keys(j),
    // The keys every V8 writes. V8 15 (oam) adds `extra_native_bytes`, drops
    // the `trace_node_id` node field and adds the `script_object_index`
    // location field; V8 12.4 (node 22) has none of those. DevTools reads the
    // layout from `meta`, so both load -- the case checks what both share.
    snapshotKeys: ["meta", "node_count", "edge_count", "trace_function_count"].every((k) => k in s),
    hasNodeFields: ["type", "name", "id", "self_size", "edge_count"].every((f) => m.node_fields.includes(f)),
    hasEdgeFields: ["type", "name_or_index", "to_node"].every((f) => m.edge_fields.includes(f)),
    nodeTypesFirstIsList: Array.isArray(m.node_types[0]),
    edgeTypesFirstIsList: Array.isArray(m.edge_types[0]),
    nodesMatch: j.nodes.length === s.node_count * nf,
    edgesMatch: j.edges.length === s.edge_count * ef,
    edgeCountsAgree: edgesFromNodes === s.edge_count,
    // What a DevTools-style loader dereferences: every edge's to_node is the
    // offset of a node, and every node's type and name index into the type
    // list and the string table.
    edgeTargetsValid: j.edges.every((v, i) => i % ef !== m.edge_fields.indexOf("to_node") || (v % nf === 0 && v < j.nodes.length)),
    nodeIndexesValid: j.nodes.every((v, i) =>
      (i % nf !== m.node_fields.indexOf("type") || v < m.node_types[0].length) &&
      (i % nf !== m.node_fields.indexOf("name") || v < j.strings.length)),
    nonEmpty: s.node_count > 0 && j.strings.length > 0,
    stringsAreStrings: j.strings.every((x) => typeof x === "string"),
    firstNodeIsRoot: j.strings[j.nodes[m.node_fields.indexOf("name")]] === "",
  }));
}

// getHeapSnapshot: the stream.
const stream = getHeapSnapshot();
console.log(
  stream.constructor.name,
  stream instanceof Readable,
  stream.readableObjectMode,
  stream.readableEncoding,
  getHeapSnapshot.length,
  writeHeapSnapshot.length,
  v8.getHeapSnapshot === getHeapSnapshot,
);
const chunks = [];
const kinds = new Set();
for await (const chunk of stream) {
  chunks.push(chunk);
  kinds.add(Buffer.isBuffer(chunk));
}
console.log("chunks are Buffers:", [...kinds].join(), "ended:", stream.readableEnded);
checkShape("stream", Buffer.concat(chunks).toString());
console.log("with options:", getHeapSnapshot({ exposeInternals: true, exposeNumericValues: false }).constructor.name);

// writeHeapSnapshot: the generated name, then each path form.
const pattern = /^Heap\.\d{8}\.\d{6}\.(\d+)\.(\d+)\.(\d{3})\.heapsnapshot$/;
const showName = (name) => {
  const m = pattern.exec(name);
  return m ? `Heap.<date>.<time>.${m[1] === String(process.pid) ? "<pid>" : m[1]}.${m[2]}.${m[3]}.heapsnapshot` : `unexpected ${name}`;
};
const first = writeHeapSnapshot();
console.log(showName(first), fs.existsSync(first));
checkShape("file", fs.readFileSync(first, "utf8"));
console.log(writeHeapSnapshot("named.heapsnapshot"), fs.statSync("named.heapsnapshot").size > 0);
const asBuffer = Buffer.from("buf.heapsnapshot");
const back = writeHeapSnapshot(asBuffer);
console.log(back === asBuffer, fs.existsSync("buf.heapsnapshot"));
const url = pathToFileURL(path.join(dir, "url.heapsnapshot"));
console.log(writeHeapSnapshot(url) === path.join(dir, "url.heapsnapshot"), fs.existsSync("url.heapsnapshot"));
// An explicit name takes no sequence number; the next generated one does.
console.log(showName(writeHeapSnapshot(undefined, { exposeNumericValues: true })));

// The worker continues the same sequence and names its own threadId.
fs.writeFileSync(
  "w.cjs",
  "const v8 = require('node:v8');\n" +
    "const { parentPort } = require('node:worker_threads');\n" +
    "parentPort.postMessage(v8.writeHeapSnapshot());\n",
);
const worker = new Worker(path.join(dir, "w.cjs"));
const [fromWorker] = await new Promise((resolve) => worker.once("message", (m) => resolve([m])));
await new Promise((resolve) => worker.once("exit", resolve));
console.log("worker", showName(fromWorker), fs.existsSync(fromWorker));
await worker.getHeapSnapshot().then(
  (s) => console.log("resolved", s.constructor.name),
  (er) => console.log(er.constructor.name, er.code, er.message),
);
console.log(showName(writeHeapSnapshot()));

// Errors, as node throws them.
const show = (fn) => {
  try {
    console.log("returned", fn());
  } catch (er) {
    console.log(er.constructor.name, er.code, er.syscall ?? "", er.message.replaceAll(dir, "<dir>"));
  }
};
show(() => writeHeapSnapshot(1));
show(() => writeHeapSnapshot(null));
show(() => writeHeapSnapshot("a\u0000b"));
show(() => writeHeapSnapshot(undefined, null));
show(() => writeHeapSnapshot(undefined, 5));
show(() => writeHeapSnapshot(undefined, []));
show(() => writeHeapSnapshot(undefined, { exposeInternals: 1 }));
show(() => getHeapSnapshot("x"));
show(() => getHeapSnapshot({ exposeNumericValues: "yes" }).constructor.name);
show(() => writeHeapSnapshot(path.join(dir, "missing", "x.heapsnapshot")));
show(() => writeHeapSnapshot(""));
show(() => worker.getHeapSnapshot(1));

process.chdir(home);
fs.rmSync(dir, { recursive: true, force: true });
