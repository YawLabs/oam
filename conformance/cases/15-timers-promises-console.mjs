// timers/promises + the console module.
import { setTimeout as wait, setInterval as every, scheduler } from "node:timers/promises";
import consoleModule, { Console } from "node:console";
import { Writable } from "node:stream";

console.log(await wait(5, "waited"));
await scheduler.wait(1);
await scheduler.yield();
console.log("scheduler-ok");
let ticks = 0;
for await (const v of every(2, "tick")) {
  if (++ticks === 3) {
    console.log(v, ticks);
    break;
  }
}
console.log(consoleModule.log === console.log);
let captured = "";
const sink = new Writable({
  write(chunk, _e, cb) {
    captured += chunk;
    cb();
  },
});
const custom = new Console(sink, sink);
custom.log("to-sink %d", 7);
custom.error("err-line");
await wait(1);
console.log(JSON.stringify(captured));
