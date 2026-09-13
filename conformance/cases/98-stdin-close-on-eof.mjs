// What process.stdin emits when its input runs out, per kind of stdin.
//
// node builds stdin's class from what fd 0 is, and the class decides what EOF
// does:
//
// - a PIPE is a net.Socket, which destroys itself after 'end': 'end', then
//   'close', and the stream reads destroyed. oam emitted 'end' and stopped
//   there, so a program that shuts down on stdin 'close' never did -- the
//   shape of an MCP server whose host closes its stdin
//   (`process.stdin.on("close", () => server.close())` in
//   @modelcontextprotocol/server-puppeteer), which then leaked its browser.
// - a FILE -- a redirect, or the null device `stdio: 'ignore'` attaches -- is
//   an fs.ReadStream opened with autoClose: false: 'end' and nothing after it,
//   never destroyed. oam already agreed, and must keep agreeing.
// - destroy() emits 'close' on either kind, exactly once however many times
//   it is called, and so does `for await (...) break`, which destroys the
//   stream on its way out -- including while the parent still holds the pipe
//   open, before any EOF.
//
// Each shape is a child on THIS runtime. It reports the stdin events it saw,
// in order, from its 'exit' handler -- so an event that never fires, or one
// that fires twice, shows up in the line. A child still running well after
// its stdin has closed is killed and reported as hung rather than hanging the
// case. HOW SOON a destroyed stdin lets a held-open pipe's process exit is
// case 97's question, not this one's: for these children node 22 on Windows
// answered it both ways across three consecutive runs, so a timing verdict
// here would flip on node alone.
import { spawn } from "node:child_process";
import { closeSync, mkdirSync, openSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-stdin-eof-${process.pid}`);
mkdirSync(dir, { recursive: true });
const input = path.join(dir, "input.txt");
writeFileSync(input, "x\n");

// How long a held-open pipe stays open before the parent closes it. A child
// still alive HUNG_MS after that is not going to exit on its own.
const HOLD_MS = 2500;
const HUNG_MS = 10000;

// The recorder every child starts with. A run of 'data' events collapses to
// one entry: how the bytes are chunked is not the question.
const RECORDER = `
const events = [];
const note = (e) => { if (!(e === "data" && events[events.length - 1] === "data")) events.push(e); };
const s = process.stdin;
s.on("end", () => note("end"));
s.on("close", () => note("close"));
s.on("error", (e) => note("error " + e.code));
process.on("exit", () => {
  process.stdout.write(JSON.stringify(events) + " destroyed=" + s.destroyed + "\\n");
});
process.stdout.write("R\\n");
`;

// [name, stdin kind, hold the pipe open?, body]
const shapes = [
  ["pipe: data", "pipe", false, "s.on('data', () => note('data'));"],
  [
    "pipe: readable + read()",
    "pipe",
    false,
    "s.on('readable', () => { while (s.read() !== null) note('data'); });",
  ],
  [
    "pipe: shut down on close",
    "pipe",
    false,
    "s.on('data', () => note('data')); s.on('close', () => setTimeout(() => note('shutdown'), 20));",
  ],
  [
    "pipe: destroy() in 'end'",
    "pipe",
    false,
    "s.on('data', () => note('data')); s.on('end', () => { s.destroy(); s.destroy(); });",
  ],
  [
    "pipe: destroy() before EOF",
    "pipe",
    true,
    "s.once('data', () => { note('data'); s.destroy(); s.destroy(); });",
  ],
  [
    "pipe: for await + break",
    "pipe",
    true,
    "(async () => { for await (const c of s) { note('chunk'); break; } note('after loop'); })();",
  ],
  ["file: data", "file", false, "s.on('data', () => note('data'));"],
  [
    "file: readable + read()",
    "file",
    false,
    "s.on('readable', () => { while (s.read() !== null) note('data'); });",
  ],
  [
    "file: destroy() before EOF",
    "file",
    false,
    "s.once('data', () => { note('data'); s.destroy(); });",
  ],
  ["ignore: data", "ignore", false, "s.on('data', () => note('data'));"],
];

for (const [name, kind, hold, body] of shapes) {
  const file = path.join(dir, "child.cjs");
  writeFileSync(file, `${RECORDER}\n${body}\n`);
  let stdin0 = kind;
  if (kind === "file") stdin0 = openSync(input, "r");
  const child = spawn(process.execPath, [file], { stdio: [stdin0, "pipe", "inherit"] });
  if (kind === "file") closeSync(stdin0);
  let out = "";
  const ready = new Promise((resolve) => {
    child.stdout.on("data", (chunk) => {
      out += chunk;
      if (out.startsWith("R\n")) resolve();
    });
  });
  const exited = new Promise((done) => child.on("close", (code) => done(code)));
  await ready;
  let holdTimer;
  if (kind === "pipe") {
    child.stdin.write("x\n");
    if (hold) {
      holdTimer = setTimeout(() => {
        try {
          child.stdin.end();
        } catch {
          /* already gone */
        }
      }, HOLD_MS);
    } else {
      child.stdin.end();
    }
  }
  let hung = false;
  const watchdog = setTimeout(() => {
    hung = true;
    child.kill();
  }, (hold ? HOLD_MS : 0) + HUNG_MS);
  const code = await exited;
  clearTimeout(watchdog);
  clearTimeout(holdTimer);
  const report = out.slice(2).trim();
  console.log(name.padEnd(28), hung ? "hung, killed" : `exit ${code}`, report);
}

rmSync(dir, { recursive: true, force: true });
