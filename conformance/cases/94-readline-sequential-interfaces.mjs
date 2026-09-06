// Two readline interfaces over ONE input, one after the other: the second is
// created after the first was close()d, and each must receive its answer.
//
// The shape is every CLI that prompts, closes the interface, and later
// prompts again (a trust prompt, then a session picker). oam's interface
// paused the input on close() -- as node's does -- but never resumed it in
// the constructor, which node does last thing. A paused Readable is not
// restarted by a new 'data' listener, so the second interface starved: the
// answer sat echoed on the terminal and nothing ran. close() also left its
// 'data'/'end' listeners attached, so the closed interface stayed a consumer.
//
// Two inputs: an in-process Readable (the listener/flowing bookkeeping is
// observable) and a real pipe into a child on the SAME runtime, answered
// only after each prompt is seen, so the lines never coalesce into one
// chunk. The child is the readline/promises variant of the same sequence.
import { spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";
import { Readable } from "node:stream";

// A starved interface would hang the case; fail it fast and loudly instead.
const watchdog = setTimeout(() => {
  console.log("TIMEOUT: a readline interface never received its answer");
  process.exit(1);
}, 8000);

function ask(rl, prompt) {
  return new Promise((resolve) => rl.question(prompt, resolve));
}

// ---- 1) in-process Readable -----------------------------------------------
{
  const input = new Readable({ read() {} });
  const first = readline.createInterface({ input });
  const events = [];
  first.on("pause", () => events.push("pause"));
  first.on("close", () => events.push("close"));
  const a1 = ask(first, "");
  input.push("one\n");
  console.log("first answer", JSON.stringify(await a1));
  first.close();
  console.log("first close events", JSON.stringify(events));
  console.log("listeners after close", input.listenerCount("data"), input.listenerCount("end"));
  console.log("flowing after close", input.readableFlowing);

  const second = readline.createInterface({ input });
  console.log("flowing after second interface", input.readableFlowing);
  const a2 = ask(second, "");
  input.push("two\n");
  console.log("second answer", JSON.stringify(await a2));
  second.close();
  console.log("listeners after both closed", input.listenerCount("data"), input.listenerCount("end"));
}

// ---- 2) real pipe into a child on this runtime -----------------------------
{
  const dir = path.join(os.tmpdir(), `oam-conf-readline-seq-${process.pid}`);
  mkdirSync(dir, { recursive: true });
  const child = path.join(dir, "child.mjs");
  writeFileSync(
    child,
    [
      "import * as readline from 'node:readline/promises';",
      "function prompter() {",
      "  let rl = null;",
      "  return {",
      "    ask(p) {",
      "      if (rl === null) rl = readline.createInterface({ input: process.stdin, output: process.stdout });",
      "      return rl.question(p);",
      "    },",
      "    close() { rl?.close(); rl = null; },",
      "  };",
      "}",
      "const a = prompter();",
      "const r1 = await a.ask('Q1? ');",
      "console.log('\\n[got1]', JSON.stringify(r1));",
      "const r2 = await a.ask('Q2? ');",
      "console.log('\\n[got2]', JSON.stringify(r2));",
      "a.close();",
      "console.log('[closed first]');",
      "const b = prompter();",
      "const r3 = await b.ask('Q3? ');",
      "console.log('\\n[got3]', JSON.stringify(r3));",
      "b.close();",
      "",
    ].join("\n"),
  );

  const cp = spawn(process.execPath, [child], { stdio: ["pipe", "pipe", "pipe"] });
  let out = "";
  let err = "";
  cp.stderr.on("data", (c) => {
    err += c.toString();
  });
  // Answer each prompt only once it has been printed, so the three lines
  // reach the child as three separate reads (node behaves the same way when
  // all three arrive in one chunk: the extra lines are emitted with no
  // question waiting and are lost).
  const script = [
    ["Q1? ", "\n"],
    ["Q2? ", "\n"],
    ["Q3? ", "2\n"],
  ];
  let seen = 0;
  cp.stdout.on("data", (c) => {
    out += c.toString();
    while (seen < script.length && out.includes(script[seen][0])) {
      cp.stdin.write(script[seen][1]);
      seen += 1;
    }
    // EOF after the last answer. The interfaces are closed by then, but a
    // paused stdin only lets the CHILD exit on node; under oam the pending
    // stdin read holds the loop until the pipe closes (a separate
    // divergence, not this case), so the pipe is closed on both to keep the
    // compared output about the answers.
    if (seen === script.length) cp.stdin.end();
  });
  const code = await new Promise((done) => cp.on("close", done));
  const lines = out.split(/\r?\n/).map((l) => l.trim()).filter((l) => l.startsWith("["));
  console.log("child lines", JSON.stringify(lines));
  console.log("child exit", code, err.trim() === "" ? "" : `stderr: ${err.trim()}`);
  rmSync(dir, { recursive: true, force: true });
}

clearTimeout(watchdog);
