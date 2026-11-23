// util.getCallSites(frameCount) validates frameCount as node's
// validateInteger(frameCount, "frameCount", 1, 200), pinned against node on
// the same host: 1.5, NaN and Infinity are "It must be an integer", not a
// range failure, and not accepted. oam checked only the range, so 1.5 got
// through and NaN/Infinity reported the range.
import util from "node:util";

for (const frameCount of [1.5, NaN, Infinity, -Infinity, 0, 201, -1, 2 ** 60, "x", 1, 200]) {
  try {
    const sites = util.getCallSites(frameCount);
    console.log(String(frameCount), "returned an array", Array.isArray(sites), sites.length <= frameCount);
  } catch (err) {
    console.log(String(frameCount), err.name, err.code, err.message);
  }
}
// The options are checked first.
try {
  util.getCallSites(1.5, { sourceMap: 1 });
} catch (err) {
  console.log("options first", err.code, err.message);
}
