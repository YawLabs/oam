// node:util fidelity: format() no-arg + negative-zero, isDeepStrictEqual on
// boxed primitives / symbol keys / typed-array extra props, stripVTControlChars
// over CSI and OSC (BEL + 7-bit/8-bit ST terminators), styleText validation,
// and inherits / deprecate argument validation.
import util from "node:util";

const ESC = String.fromCharCode(0x1b);
const BEL = String.fromCharCode(0x07);
const ST7 = ESC + "\\"; // 7-bit string terminator
const ST8 = String.fromCharCode(0x9c); // 8-bit string terminator

const eq = (label, a, b) => console.log(label + "=" + (a === b));
const tag = (label, fn) => {
  try {
    fn();
    console.log(label + ":ok");
  } catch (e) {
    console.log(label + ":throw:" + (e.code || e.name));
  }
};

// format: zero-arg is '', and negative zero keeps its sign.
console.log("fmt-empty " + JSON.stringify(util.format()));
console.log("fmt-arr " + JSON.stringify(util.format([])));
console.log("fmt-d-negzero " + JSON.stringify(util.format("%d", -0)));
console.log("fmt-s-negzero " + JSON.stringify(util.format("%s", -0)));

// isDeepStrictEqual: boxed primitives, symbols, typed-array extra props.
eq("ide-box-ne", util.isDeepStrictEqual(new Number(2), new Number(1)), false);
eq("ide-box-eq", util.isDeepStrictEqual(new Boolean(true), new Boolean(true)), true);
const s1 = Symbol("k");
eq("ide-sym-eq", util.isDeepStrictEqual({ [s1]: 1 }, { [s1]: 1 }), true);
eq("ide-sym-ne", util.isDeepStrictEqual({ [s1]: 1 }, { [Symbol("k")]: 1 }), false);
const ta1 = new Uint8Array(2);
const ta2 = new Uint8Array(2);
ta1[s1] = true;
ta2[s1] = false;
eq("ide-ta-prop", util.isDeepStrictEqual(ta1, ta2), false);

// stripVTControlCharacters: CSI colors + OSC hyperlinks (all 3 terminators).
console.log(
  "strip-csi " +
    JSON.stringify(util.stripVTControlCharacters(ESC + "[31mhi" + ESC + "[39m")),
);
for (const [name, st] of [["bel", BEL], ["st7", ST7], ["st8", ST8]]) {
  const link = ESC + "]8;;https://x.test/?a=1&b=2" + st + "go" + ESC + "]8;;" + st;
  console.log("strip-osc-" + name + " " + JSON.stringify(util.stripVTControlCharacters(link)));
}

// styleText: validates format and text; colorizes with validateStream:false.
tag("style-badfmt", () => util.styleText("nope", "x"));
tag("style-badtext", () => util.styleText("red", 5));
console.log(
  "style-red " +
    JSON.stringify(util.styleText("red", "x", { validateStream: false })),
);

// inherits / deprecate argument validation.
tag("inherits-bad", () => util.inherits(function () {}, {}));
tag("deprecate-badcode", () => util.deprecate(() => {}, "msg", 1));
