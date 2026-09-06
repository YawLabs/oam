// readline line terminators. node's lineEnding is /\r?\n|\r(?!\n)/: a lone
// "\r" ends a line (progress bars write "10%\r20%\r"), and a "\r" that
// closes one chunk followed by a "\n" opening the next is ONE terminator if
// the "\n" arrives within crlfDelay ms, a fresh (empty) line beyond it.
// crlfDelay clamps up to 100 and accepts Infinity.
//
// oam split on /\r?\n/ only: a "\r"-terminated line did not surface until
// the next "\n" arrived (and then with the "\r" glued on), and crlfDelay was
// stored but never consulted.
import readline from "node:readline";
import { Readable } from "node:stream";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function lines(label, opts, feed) {
  const input = new Readable({ read() {} });
  const rl = readline.createInterface({ input, ...opts });
  const got = [];
  rl.on("line", (l) => got.push(l));
  await feed(input);
  await sleep(20);
  rl.close();
  console.log(label, JSON.stringify(got), "crlfDelay", rl.crlfDelay);
}

await lines("cr inside a chunk", {}, async (i) => {
  i.push("a\rb\n");
});
await lines("cr-only terminators", {}, async (i) => {
  i.push("10%\r20%\r");
});
// The two halves of a "\r\n" land in separate chunks. Wide margins on both
// sides of the delay so a loaded box cannot flip either verdict.
await lines("split beyond crlfDelay", { crlfDelay: 100 }, async (i) => {
  i.push("x\r");
  await sleep(300);
  i.push("\ny\n");
});
await lines("split within crlfDelay", { crlfDelay: 5000 }, async (i) => {
  i.push("x\r");
  await sleep(10);
  i.push("\ny\n");
});
await lines("split under Infinity", { crlfDelay: Infinity }, async (i) => {
  i.push("x\r");
  await sleep(150);
  i.push("\ny\n");
});
await lines("crlfDelay clamps up to 100", { crlfDelay: 5 }, async (i) => {
  i.push("q\r\n");
});
await lines("buffered head then crlf", {}, async (i) => {
  i.push("abc");
  i.push("\r\nz");
  i.push("\n");
});
await lines("cr closes the last chunk", {}, async (i) => {
  i.push("z\r");
  i.push(null);
});
await lines("unterminated tail at end", {}, async (i) => {
  i.push("tail");
  i.push(null);
});
