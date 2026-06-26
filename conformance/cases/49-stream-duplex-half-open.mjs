// Duplex half-open: a Duplex defaults allowHalfOpen=true; when set false, the
// readable side hitting EOF auto-ends the writable side (-> 'finish'). Setting
// _writableState.ended beforehand suppresses the auto-end. Mirrors Node's
// test-stream-duplex-end.
import { Duplex } from "node:stream";

function run(label, opts, preEndWritable) {
  return new Promise((resolve) => {
    const stream = new Duplex({ read() {}, ...opts });
    let ended = false;
    let finished = false;
    stream.on("end", () => { ended = true; });
    stream.on("finish", () => { finished = true; });
    if (preEndWritable) stream._writableState.ended = true;
    stream.resume();
    stream.push(null);
    setTimeout(() => {
      console.log(
        label +
          " allowHalfOpen=" + stream.allowHalfOpen +
          " end=" + ended +
          " finish=" + finished,
      );
      resolve();
    }, 25);
  });
}

await run("default", {}, false);
await run("halfopen-false", { allowHalfOpen: false }, false);
await run("preended", { allowHalfOpen: false }, true);
