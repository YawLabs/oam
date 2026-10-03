// performance.markResourceTiming: how a fetch implementation reports a fetch
// as a 'resource' entry. The npm undici's fetch calls it for every response;
// oam had none, so that fetch threw "markResourceTiming is not a function"
// once a response's body was read (#206).
import { performance as hooksPerformance } from "node:perf_hooks";

console.log(typeof performance.markResourceTiming, performance.markResourceTiming.length);
const timing = { startTime: 10, endTime: 35.5, finalServiceWorkerStartTime: 0, encodedBodySize: 0 };
const entry = performance.markResourceTiming(timing, "http://example.invalid/r", "fetch", globalThis, "", {}, 200);
console.log(entry.name, entry.entryType, entry.startTime, entry.duration, entry.initiatorType, entry.responseStatus);
const listed = hooksPerformance.getEntriesByType("resource");
console.log(listed.length, listed[0].name);
hooksPerformance.clearResourceTimings();
console.log(hooksPerformance.getEntriesByType("resource").length);
