// A forked child that sends and then disconnects at once, before its channel
// has finished opening: node delivers what was sent, then closes.
//
// oam's child connects its channel lazily, so the messages waited in a queue;
// disconnect() closed the socket that was still connecting and dropped them,
// and the parent heard nothing.
import { fork } from "node:child_process";
import { fileURLToPath } from "node:url";

if (process.argv[2] === "child") {
  let disconnects = 0;
  process.on("disconnect", () => disconnects++);
  process.send({ n: 1 });
  process.send({ n: 2 });
  process.disconnect();
  // The exit code reports whether 'disconnect' fired once.
  process.on("exit", () => {
    process.exitCode = disconnects === 1 ? 7 : 8;
  });
} else {
  const got = [];
  const cp = fork(fileURLToPath(import.meta.url), ["child"]);
  cp.on("message", (m) => got.push(m));
  cp.on("exit", (code) => {
    console.log("messages", JSON.stringify(got));
    console.log("child exit", code);
  });
}
