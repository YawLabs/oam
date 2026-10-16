// Each fs.watch event is its own callback, as node's are (libuv calls
// FSEvent's onchange through MakeCallback, once per event): the nextTick
// queue and then the microtasks drain after every listener call, and a
// listener's throw is an uncaught exception -- the remaining events still
// arrive. Also fs.promises.watch's own name, and the start error naming a
// file: URL as given.
//
// The OS decides how many events a burst of writes becomes, so nothing here
// prints a count or a sequence: each check prints whether a property held
// across however many events arrived (at least two, waited for).
//
// Regression guard: oam delivered a burst of events in one promise reaction.
// A listener's nextTick ran after its promise jobs, the queued work of every
// event ran only after the last listener call, and a throw became an
// 'unhandledRejection' that also dropped the rest of the burst.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

const root = fs.mkdtempSync(path.join(os.tmpdir(), "oam-422-"));

// Resolve once `count()` reaches `n`, writing a new file into `dir` every
// 100 ms until it does; give up after 10 s.
function writeUntil(dir, count, n) {
  return new Promise((resolve, reject) => {
    let i = 0;
    const timer = setInterval(() => {
      if (count() >= n) {
        clearInterval(timer);
        resolve();
      } else if (++i > 100) {
        clearInterval(timer);
        reject(new Error("too few events"));
      } else {
        // Three files per round: one burst, several events at once.
        for (let k = 0; k < 3; k++) fs.writeFileSync(path.join(dir, `f${i}-${k}`), "x");
      }
    }, 100);
  });
}

// Between two listener calls, the first call's tick and then its microtask
// have run.
{
  const dir = path.join(root, "order");
  fs.mkdirSync(dir);
  const log = [];
  let events = 0;
  const w = fs.watch(dir, () => {
    const n = ++events;
    log.push(`ev${n}`);
    process.nextTick(() => log.push(`tick${n}`));
    Promise.resolve().then(() => log.push(`micro${n}`));
  });
  await writeUntil(dir, () => events, 6);
  w.close();
  let interleaved = true;
  for (let n = 1; n < events; n++) {
    const at = log.indexOf(`ev${n}`);
    if (log[at + 1] !== `tick${n}` || log[at + 2] !== `micro${n}`) interleaved = false;
  }
  console.log("tick then microtask after every event:", interleaved);
}

// A listener's throw reaches 'uncaughtException', once per call, and the
// events after it are still delivered.
{
  const dir = path.join(root, "throw");
  fs.mkdirSync(dir);
  let calls = 0;
  let uncaught = 0;
  let rejections = 0;
  const onUncaught = (e) => {
    if (e.message === "listener threw") uncaught++;
  };
  const onRejection = () => rejections++;
  process.on("uncaughtException", onUncaught);
  process.on("unhandledRejection", onRejection);
  const w = fs.watch(dir, () => {
    calls++;
    throw new Error("listener threw");
  });
  await writeUntil(dir, () => calls, 6);
  w.close();
  // Give a late rejection report the chance to show.
  await new Promise((r) => setTimeout(r, 50));
  process.removeListener("uncaughtException", onUncaught);
  process.removeListener("unhandledRejection", onRejection);
  console.log("every throw uncaught:", calls >= 6 && uncaught === calls, "rejections:", rejections);
}

// fs.promises.watch is named `watch`; a failed start names a file: URL as
// given, with the URL itself as `filename`.
{
  console.log("fs.promises.watch:", fsp.watch.name, fsp.watch.length);
  const url = pathToFileURL(path.join(root, "missing"));
  try {
    for await (const _ of fsp.watch(url)) break;
  } catch (e) {
    console.log(
      e.code,
      e.syscall,
      e.message === `ENOENT: no such file or directory, watch '${url.href}'`,
      e.path === url.href,
      e.filename === url,
      Object.keys(e).join(","),
    );
  }
}

fs.rmSync(root, { recursive: true, force: true });
