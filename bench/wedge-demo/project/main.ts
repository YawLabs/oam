// The wedge-demo program. One line below carries the classic silent
// JavaScript bug: a string where a number was declared. After type
// stripping it RUNS on every runtime — and prints the wrong answer
// ("410" instead of 14, courtesy of string concatenation). Node executes
// it silently. Bun executes it silently. oam executes it instantly AND
// tells you — and tells your agent, machine-readably.
import { mean } from "@lib/stats";

const raw: string = await oam.readTextFile("./data.json");
const values: number[] = JSON.parse(raw);

const padding: number = "10"; // <- the planted bug (TS2322)

const total = mean(values) + padding;
console.log("mean:", mean(values));
console.log("total:", total);
