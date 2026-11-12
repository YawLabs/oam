// node:assert / node:util contracts that only show on the less common paths:
// deep equality of Sets and Maps whose members must be matched pairwise (a
// pair that compared unequal once must compare unequal again), a format call
// made from inside an inspect hook, assert.match on a non-string, the custom
// message of assert.throws for a validation function and for a caught
// AssertionError, util.types.isKeyObject, and util.parseArgs.
import assert from "node:assert";
import crypto from "node:crypto";
import os from "node:os";
import path from "node:path";
import util from "node:util";

const tag = (label, fn) => {
  try {
    fn();
    console.log(label + ":ok");
  } catch (e) {
    console.log(label + ":throw:" + (e.code || e.name));
  }
};
const failure = (label, fn) => {
  try {
    fn();
    console.log(label, "NO THROW");
  } catch (e) {
    console.log(label, JSON.stringify({ name: e.name, code: e.code, message: e.message, generated: e.generatedMessage, operator: e.operator }));
  }
};

// --- deep equality: Set / Map members matched pairwise
{
  const x = { v: 1 };
  const y = { v: 2 };
  tag("set-retry-unequal", () => assert.deepStrictEqual(new Set([[x], [x]]), new Set([[y], [x]])));
  tag("set-retry-loose", () => assert.deepEqual(new Set([[x], [x]]), new Set([[y], [x]])));
  tag("map-retry-unequal", () => assert.deepStrictEqual(new Map([[[x], 1], [[x], 1]]), new Map([[[y], 1], [[x], 1]])));
  tag("partial-retry-unequal", () => assert.partialDeepStrictEqual(new Set([[x], [x]]), new Set([[y], [x]])));
  console.log("is-retry-unequal=" + util.isDeepStrictEqual(new Set([[x], [x]]), new Set([[y], [x]])));
  tag("set-objects-equal", () => assert.deepStrictEqual(new Set([[x], [y]]), new Set([[{ v: 2 }], [{ v: 1 }]])));
  tag("set-prims-reordered", () => assert.deepStrictEqual(new Set([1, "a", null, undefined, NaN, 2n]), new Set([2n, NaN, undefined, null, "a", 1])));
  tag("set-prim-missing", () => assert.deepStrictEqual(new Set([1, 2]), new Set([1, 3])));
  tag("set-prim-vs-object", () => assert.deepStrictEqual(new Set([1, [2]]), new Set([[2], [1]])));
  tag("set-loose-coerced", () => assert.deepEqual(new Set([1, "2"]), new Set(["1", 2])));
  tag("set-loose-identity-first", () => assert.deepEqual(new Set([0, ""]), new Set([0, "0"])));
  tag("set-strict-coerced", () => assert.deepStrictEqual(new Set([1]), new Set(["1"])));
  tag("map-prims-reordered", () => assert.deepStrictEqual(new Map([[1, "a"], ["k", { z: 1 }]]), new Map([["k", { z: 1 }], [1, "a"]])));
  tag("map-prim-value-differs", () => assert.deepStrictEqual(new Map([[1, "a"]]), new Map([[1, "b"]])));
  tag("map-prim-key-missing", () => assert.deepStrictEqual(new Map([[1, "a"]]), new Map([[2, "a"]])));
  const o = { k: 1 };
  const p = { k: 1 };
  tag("map-object-keys-crossed", () => assert.deepStrictEqual(new Map([[o, 1], [p, 2]]), new Map([[o, 2], [p, 1]])));
  tag("map-loose-coerced-key", () => assert.deepEqual(new Map([[1, "a"]]), new Map([["1", "a"]])));
  tag("map-undefined-value", () => assert.deepStrictEqual(new Map([[1, undefined]]), new Map([[2, undefined]])));
  // A large Set of primitives in a different order compares in linear time.
  const many = Array.from({ length: 60000 }, (_, i) => i);
  tag("set-large-reversed", () => assert.deepStrictEqual(new Set(many), new Set(many.slice().reverse())));
  tag("map-large-reversed", () => assert.deepStrictEqual(new Map(many.map((i) => [i, i])), new Map(many.slice().reverse().map((i) => [i, i]))));
  // Cycles still terminate.
  const c1 = { name: "c" };
  c1.self = c1;
  const c2 = { name: "c" };
  c2.self = c2;
  tag("cycle-equal", () => assert.deepStrictEqual(c1, c2));
  tag("cycle-in-set", () => assert.deepStrictEqual(new Set([c1]), new Set([c2])));
}

// --- util.format called from inside an inspect hook keeps the outer options
{
  const hooked = { [util.inspect.custom]: () => util.format("%s", "inner") };
  console.log("fmt-reentrant " + JSON.stringify(util.formatWithOptions({ colors: true }, "%O %O", hooked, { a: 1 })));
  console.log("fmt-reentrant-sep " + JSON.stringify(util.formatWithOptions({ numericSeparator: true }, "%O %d", hooked, 1234567)));
  console.log("fmt-default-after " + JSON.stringify(util.format("%O %d", { a: 1 }, 1234567)));
}

// --- assert.match / doesNotMatch
failure("match-number", () => assert.match(123, /2/));
failure("match-undefined", () => assert.match(undefined, /x/));
failure("doesNotMatch-undefined", () => assert.doesNotMatch(undefined, /x/));
failure("doesNotMatch-number", () => assert.doesNotMatch(123, /9/));
failure("match-object", () => assert.match({ toString() { return "abc"; } }, /b/));
failure("match-fail", () => assert.match("abc", /x/));
failure("doesNotMatch-fail", () => assert.doesNotMatch("abc", /b/));
failure("match-fail-custom", () => assert.match("abc", /x/, "custom"));
failure("doesNotMatch-fail-custom", () => assert.doesNotMatch("abc", /b/, "custom"));
failure("match-fail-error", () => assert.match("abc", /x/, new RangeError("mine")));
failure("match-number-custom", () => assert.match(123, /2/, "custom"));
failure("match-bad-regexp", () => assert.match("abc", "b"));
failure("doesNotMatch-bad-regexp", () => assert.doesNotMatch("abc", "b"));
failure("strict-match-number", () => assert.strict.match(5, /5/));
tag("match-ok", () => assert.match("abc", /b/));
tag("doesNotMatch-ok", () => assert.doesNotMatch("abc", /x/));
console.log("match-names " + assert.match.name + " " + assert.doesNotMatch.name);

// --- assert.throws / rejects with a custom message
{
  const boom = () => {
    throw new Error("x");
  };
  failure("throws-validator-custom", () => assert.throws(boom, () => false, "custom"));
  failure("throws-validator-generated", () => assert.throws(boom, () => false));
  failure("throws-inner-assertion-class", () => assert.throws(() => assert.ok(false, "inner"), TypeError, "custom"));
  failure("throws-inner-assertion-regexp", () => assert.throws(() => assert.ok(false, "inner"), /nope/, "custom"));
  failure("throws-inner-assertion-object", () => assert.throws(() => assert.ok(false, "inner"), { name: "Nope" }, "custom"));
  failure("throws-inner-assertion-validator", () => assert.throws(() => assert.ok(false, "inner"), () => false, "custom"));
  failure("throws-class-custom", () => assert.throws(boom, TypeError, "custom"));
  await assert.rejects(Promise.reject(new Error("x")), () => false, "custom").then(
    () => console.log("rejects-validator-custom NO THROW"),
    (e) => console.log("rejects-validator-custom", JSON.stringify({ message: e.message, generated: e.generatedMessage, operator: e.operator })),
  );
  await assert.rejects(Promise.reject(new assert.AssertionError({ message: "inner" })), TypeError, "custom").then(
    () => console.log("rejects-inner-assertion NO THROW"),
    (e) => console.log("rejects-inner-assertion", JSON.stringify({ message: e.message, generated: e.generatedMessage, operator: e.operator })),
  );
}

// --- util.types.isKeyObject
{
  const secret = crypto.createSecretKey(Buffer.from("0123456789abcdef"));
  console.log("isKeyObject-secret=" + util.types.isKeyObject(secret));
  console.log("isKeyObject-plain=" + util.types.isKeyObject({ type: "secret" }));
  console.log("isKeyObject-buffer=" + util.types.isKeyObject(Buffer.alloc(1)));
  console.log("isKeyObject-undefined=" + util.types.isKeyObject(undefined));
}

// --- util.parseArgs
{
  const options = {
    foo: { type: "string", short: "f" },
    bar: { type: "boolean", short: "b" },
    many: { type: "string", multiple: true, default: ["a"] },
    verbose: { type: "boolean", multiple: true, short: "v" },
    dflt: { type: "string", default: "dd" },
  };
  const run = (label, config) => {
    try {
      const r = util.parseArgs(config);
      console.log(label, JSON.stringify(r), "null-proto=" + (Object.getPrototypeOf(r.values) === null));
    } catch (e) {
      console.log(label, "THROW", e.name, e.code, JSON.stringify(e.message));
    }
  };
  run("positional-default", { args: ["x"] });
  run("positional-empty-options", { args: ["x"], options: {} });
  run("positional-loose", { args: ["x"], strict: false });
  run("positional-allowed", { args: ["x"], allowPositionals: true });
  run("inherited-name-strict", { args: ["--constructor"], options: {} });
  run("inherited-name-toString", { args: ["--toString"], options: {} });
  run("inherited-name-loose", { args: ["--constructor", "--hasOwnProperty", "--valueOf=1"], strict: false, tokens: true });
  run("proto-name", { args: ["--__proto__=1", "--__proto__"], strict: false, tokens: true });
  run("multiple-default-kept", { args: ["--many", "b"], options });
  console.log("default-array-untouched " + JSON.stringify(options.many.default));
  run("multiple-twice", { args: ["--many", "b", "--many=c"], options, tokens: true });
  run("defaults-only", { args: [], options });
  run("string-missing", { args: ["--foo"], options });
  run("string-short-missing", { args: ["-f"], options });
  run("string-ambiguous", { args: ["--foo", "-x"], options });
  run("string-short-ambiguous", { args: ["-f", "-x"], options });
  run("string-inline-dash", { args: ["--foo=-x"], options, tokens: true });
  run("boolean-with-value", { args: ["--bar=1"], options });
  run("boolean-with-value-loose", { args: ["--bar=1"], options, strict: false });
  run("short-groups", { args: ["-bvv", "-fVAL", "-bfX", "p"], options, tokens: true, allowPositionals: true });
  run("short-group-string-middle", { args: ["-vfb"], options, tokens: true });
  run("terminator", { args: ["a", "--", "--foo", "b"], options, allowPositionals: true, tokens: true });
  run("unknown", { args: ["--unknown"], options });
  run("unknown-positionals-allowed", { args: ["--unknown"], options, allowPositionals: true });
  run("unknown-short", { args: ["-z"], options });
  run("loose-unknowns", { args: ["-z", "--q=1", "--w", "e"], strict: false, tokens: true });
  run("negative", { args: ["--no-bar"], options, allowNegative: true, tokens: true });
  run("negative-off", { args: ["--no-bar"], options });
  run("negative-string", { args: ["--no-foo"], options, allowNegative: true });
  run("negative-with-value", { args: ["--no-bar=1"], options, allowNegative: true });
  run("negative-loose", { args: ["--no-zzz", "--no-foo", "v", "--no-"], allowNegative: true, strict: false, tokens: true });
  run("repeat-last-wins", { args: ["--foo", "v", "--foo", "w"], options });
  run("multiple-boolean", { args: ["--verbose", "--verbose"], options });
  run("lone-dash", { args: ["-", "--=", "--a="], strict: false, tokens: true });
  run("bad-args", { args: "x" });
  run("bad-strict", { args: [], strict: 1 });
  run("bad-tokens", { args: [], tokens: 1 });
  run("bad-allowPositionals", { args: [], allowPositionals: 1 });
  run("bad-allowNegative", { args: [], allowNegative: 1 });
  run("bad-options", { args: [], options: [] });
  run("bad-option-config", { args: [], options: { a: 1 } });
  run("bad-option-type-missing", { args: [], options: { a: {} } });
  run("bad-option-type", { args: [], options: { a: { type: "number" } } });
  run("bad-short-length", { args: [], options: { a: { type: "string", short: "ab" } } });
  run("bad-short-type", { args: [], options: { a: { type: "string", short: 1 } } });
  run("bad-multiple", { args: [], options: { a: { type: "string", multiple: 1 } } });
  run("bad-default", { args: [], options: { a: { type: "string", default: 1 } } });
  run("bad-default-not-array", { args: [], options: { a: { type: "string", multiple: true, default: "x" } } });
  run("bad-default-element", { args: [], options: { a: { type: "boolean", multiple: true, default: [1] } } });
}

// --- os.userInfo() / os.version(): the host's own answers. Windows has no
// uid, gid or shell, and its os.version() is a recorded divergence
// (docs/node-divergences.md entry 15), so that one is compared off Windows.
{
  const info = os.userInfo();
  console.log("userInfo-keys " + Object.keys(info).join(","));
  console.log("userInfo-ids " + info.uid + " " + info.gid);
  console.log("userInfo-euid=" + (process.platform === "win32" ? info.uid === -1 : info.uid === process.geteuid()));
  console.log("userInfo-name " + info.username);
  console.log("userInfo-shell " + (info.shell === null ? "null" : path.posix.basename(info.shell)));
  console.log("userInfo-home " + path.basename(info.homedir));
  console.log("version " + (process.platform === "win32" ? "skipped" : os.version()));
}
