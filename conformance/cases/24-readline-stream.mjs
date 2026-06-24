// readline.createInterface(inputStream) -- the positional-stream form (NOT the
// {input} options object). @puppeteer/browsers reads a browser subprocess's
// stderr line-by-line this way to find the CDP websocket endpoint; the
// options-only form left input unset, so no lines fired. Guards both forms.
import readline from "node:readline";
import { Readable } from "node:stream";

function collect(makeRl) {
  return new Promise((resolve) => {
    const input = new Readable({ read() {} });
    const lines = [];
    const rl = makeRl(input);
    rl.on("line", (l) => lines.push(l));
    rl.on("close", () => resolve(lines));
    input.push("alpha\nbeta\n");
    input.push("gam"); // line split across chunks
    input.push("ma\ndelta\n");
    input.push(null);
  });
}

// 1) positional stream form (the fix).
const a = await collect((input) => readline.createInterface(input));
console.log("positional", JSON.stringify(a));
// 2) options-object form (must still work).
const b = await collect((input) => readline.createInterface({ input }));
console.log("options", JSON.stringify(b));
