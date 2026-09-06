// readline question() semantics, three of them:
//
// - A line that answers a pending question goes ONLY to that question's
//   callback: no 'line' event for it, and a question asked while one is
//   pending re-prompts (the first question's prompt) and is dropped.
// - question() on a closed interface throws ERR_USE_AFTER_CLOSE ('readline
//   was closed') synchronously and writes nothing; the promises variant
//   rejects with the same. `closed` is undefined until close() sets it.
// - readline/promises question() rejects with node's AbortError -- name
//   'AbortError', code 'ABORT_ERR', cause === signal.reason, an Error, not a
//   DOMException -- for a pre-aborted signal (prompt not written) and for an
//   abort after the prompt (which ends the prompt line and hands later lines
//   back to 'line').
//
// oam fanned an answer out to every 'line' listener, wrote the prompt and
// hung on a closed interface, and rejected a DOMException (code 20).
import readline from "node:readline";
import rlp from "node:readline/promises";
import { Readable } from "node:stream";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const sink = () => {
  const out = [];
  return { out, output: { write: (s) => out.push(s) } };
};

// ---- a pending question owns its line ------------------------------------
{
  const input = new Readable({ read() {} });
  const { out, output } = sink();
  const rl = readline.createInterface({ input, output });
  const ev = [];
  rl.on("line", (l) => ev.push(`line:${l}`));
  rl.question("q1? ", (a) => ev.push(`cb1:${a}`));
  rl.question("q2? ", (a) => ev.push(`cb2:${a}`));
  input.push("x\n");
  await sleep(10);
  input.push("two\n");
  await sleep(10);
  console.log("events", JSON.stringify(ev), "output", JSON.stringify(out));
  rl.question("q3? ", (a) => ev.push(`cb3:${a}`));
  input.push("three\n");
  await sleep(10);
  console.log("events", JSON.stringify(ev), "output", JSON.stringify(out));
  console.log("prompt restored", JSON.stringify(rl.getPrompt()));
  rl.close();
}

// ---- question after close --------------------------------------------------
{
  const { out, output } = sink();
  const rl = readline.createInterface({ input: new Readable({ read() {} }), output });
  console.log("closed before", rl.closed);
  rl.close();
  console.log("closed after", rl.closed);
  try {
    rl.question("q? ", () => {});
    console.log("no throw");
  } catch (e) {
    console.log("throws", e.code, JSON.stringify(e.message), e.name, e instanceof Error, "wrote", JSON.stringify(out));
  }
  const p = rlp.createInterface({ input: new Readable({ read() {} }), output });
  p.close();
  const q = p.question("q? ");
  console.log("promises returns a promise", q instanceof Promise);
  try {
    await q;
    console.log("promises resolved?!");
  } catch (e) {
    console.log("promises rejects", e.code, JSON.stringify(e.message), "wrote", JSON.stringify(out));
  }
}

// ---- abort shapes ------------------------------------------------------------
{
  const reason = new Error("why");
  const ac = new AbortController();
  ac.abort(reason);
  const { out, output } = sink();
  const rl = rlp.createInterface({ input: new Readable({ read() {} }), output });
  try {
    await rl.question("pre? ", { signal: ac.signal });
    console.log("pre-aborted resolved?!");
  } catch (e) {
    console.log(
      "pre-aborted",
      e.name,
      e.code,
      JSON.stringify(e.message),
      "cause===reason",
      e.cause === reason,
      "Error",
      e instanceof Error,
      "DOMException",
      e instanceof DOMException,
      "wrote",
      JSON.stringify(out),
    );
  }
  rl.close();

  const ac2 = new AbortController();
  const input2 = new Readable({ read() {} });
  const { out: out2, output: output2 } = sink();
  const rl2 = rlp.createInterface({ input: input2, output: output2 });
  const ev2 = [];
  rl2.on("line", (l) => ev2.push(`line:${l}`));
  const pending = rl2.question("post? ", { signal: ac2.signal });
  await sleep(5);
  ac2.abort();
  try {
    await pending;
    console.log("post-abort resolved?!");
  } catch (e) {
    console.log(
      "post-abort",
      e.name,
      e.code,
      "cause===reason",
      e.cause === ac2.signal.reason,
      "reason name",
      ac2.signal.reason.name,
      "wrote",
      JSON.stringify(out2),
    );
  }
  input2.push("late\n");
  await sleep(10);
  console.log("line after abort", JSON.stringify(ev2));
  rl2.close();

  // The callback API: a pre-aborted signal is a silent no-op, an abort after
  // the prompt cancels the question (its callback never runs).
  const ac3 = new AbortController();
  ac3.abort();
  const input3 = new Readable({ read() {} });
  const { out: out3, output: output3 } = sink();
  const rl3 = readline.createInterface({ input: input3, output: output3 });
  const ev3 = [];
  rl3.on("line", (l) => ev3.push(`line:${l}`));
  rl3.question("never? ", { signal: ac3.signal }, (a) => ev3.push(`cb:${a}`));
  const ac4 = new AbortController();
  rl3.question("cancelled? ", { signal: ac4.signal }, (a) => ev3.push(`cb4:${a}`));
  ac4.abort();
  input3.push("after\n");
  await sleep(10);
  console.log("callback api", JSON.stringify(ev3), "wrote", JSON.stringify(out3));
  rl3.close();
}
