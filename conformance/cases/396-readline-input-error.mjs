// A readline Interface carries its input's 'error' (#337). node's Interface
// listens for 'error' on the stream it reads and emits it on itself, so the
// failure is the Interface's to report: an 'error' listener on it is called,
// and `for await (const line of rl)` -- which is what FileHandle.readLines()
// hands back -- rejects with it, after the lines that arrived first.
//
// Regression guard: oam's Interface did not listen for its input's 'error'
// at all. With nothing listening on the stream the error was thrown as an
// uncaught exception: `for await (const line of handle.readLines())` over a
// handle whose read fails ended the process, inside a try / catch that never
// saw it.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";
import readlinePromises from "node:readline/promises";
import { Readable } from "node:stream";

const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-396-"));
// Whichever row throws, the directory goes.
process.on("exit", () => {
  try {
    fs.rmSync(made, { recursive: true, force: true });
  } catch {}
});
const shape = (e) => (e && e.code ? `${e.code} ${e.syscall}` : e instanceof Error ? e.message : "threw " + String(e));
const file = path.join(made, "lines.txt");
fs.writeFileSync(file, "one\ntwo\nthree\n");

// Every line the iteration yields, then how it ends.
const drain = async (iterable) => {
  const lines = [];
  try {
    for await (const line of iterable) lines.push(line);
    return JSON.stringify(lines) + " done";
  } catch (e) {
    return JSON.stringify(lines) + " rejected " + shape(e);
  }
};
// A stream that gives `lines` and then fails.
const failing = (lines, error = new Error("input failed")) => {
  let i = 0;
  return new Readable({
    read() {
      if (i < lines.length) this.push(lines[i++]);
      else this.destroy(error);
    },
  });
};

// ---- FileHandle.readLines ----
{
  const handle = await fs.promises.open(file, "r");
  console.log("readLines r: " + (await drain(handle.readLines())) + " fd " + handle.fd);
}
{
  // Not opened for reading: the first read fails.
  const handle = await fs.promises.open(file, "a");
  console.log("readLines a: " + (await drain(handle.readLines())));
  console.log("readLines a: handle fd " + handle.fd + " close " + (await handle.close().then(() => "ok", shape)));
}
{
  const handle = await fs.promises.open(file, "a");
  const rl = handle.readLines();
  const seen = [];
  for (const name of ["line", "close", "pause", "resume"]) rl.on(name, () => seen.push(name));
  rl.on("error", (e) => seen.push("error " + shape(e)));
  await new Promise((res) => setTimeout(res, 50));
  console.log("readLines a, listening: " + JSON.stringify(seen));
  await handle.close().catch(() => {});
}
if (process.platform === "win32") {
  // A directory handle: it opens, and the read is what fails.
  const handle = await fs.promises.open(made, "r");
  console.log("readLines directory: " + (await drain(handle.readLines())));
  await handle.close().catch(() => {});
}

// ---- Any input that fails ----
for (const [label, lines] of [["at once", []], ["after two lines", ["a\n", "b\n"]], ["mid line", ["a\nb"]]]) {
  for (const [kind, make] of [["readline", (o) => readline.createInterface(o)], ["readline/promises", (o) => readlinePromises.createInterface(o)]]) {
    console.log(`${kind} ${label} | for await: ` + (await drain(make({ input: failing(lines), crlfDelay: Infinity }))));

    const rl = make({ input: failing(lines) });
    const seen = [];
    rl.on("line", (l) => seen.push("line " + l));
    rl.on("close", () => seen.push("close"));
    rl.on("error", (e) => seen.push("error " + shape(e)));
    await new Promise((res) => setTimeout(res, 30));
    console.log(`${kind} ${label} | listening: ` + JSON.stringify(seen));
    rl.close();
  }
}

// What the iterator says once it has rejected.
{
  const rl = readline.createInterface({ input: failing(["a\n"]) });
  const it = rl[Symbol.asyncIterator]();
  const steps = [];
  for (let i = 0; i < 4; i++) {
    steps.push(await it.next().then((r) => (r.done ? "done" : "line " + r.value), (e) => "rejected " + shape(e)));
  }
  console.log("iterator steps: " + JSON.stringify(steps));
  rl.close();
}
// The same iterator each time it is asked for.
{
  const rl = readline.createInterface({ input: Readable.from(["a\nb\n"]) });
  console.log("one iterator: " + (rl[Symbol.asyncIterator]() === rl[Symbol.asyncIterator]()));
  console.log("plain input: " + (await drain(rl)));
}
// An error that is no Error, and one emitted without destroying the stream.
{
  const rl = readline.createInterface({ input: failing([], "a string") });
  console.log("string error: " + (await drain(rl)));
}
{
  const input = new Readable({ read() {} });
  const rl = readline.createInterface({ input });
  const answer = drain(rl);
  input.push("kept\n");
  await new Promise((res) => setImmediate(res));
  input.emit("error", new Error("emitted"));
  console.log("emitted, not destroyed: " + (await answer) + " input destroyed " + input.destroyed);
  rl.close();
}
// An input error with nobody listening on the Interface is uncaught.
{
  const caught = await new Promise((res) => {
    process.once("uncaughtException", (e) => res("uncaught " + shape(e)));
    readline.createInterface({ input: failing([]) });
    setTimeout(() => res("nothing"), 100);
  });
  console.log("no listener: " + caught);
}
console.log("still running");
