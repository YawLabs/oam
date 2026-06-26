// process.env is a string-coercing view: non-string assignment coerces via
// String() (undefined -> 'undefined', NOT delete); symbol keys are rejected
// the way Node does; delete always reports true; defineProperty only accepts a
// configurable+writable+enumerable data descriptor.
const tag = (label, fn) => {
  try {
    const v = fn();
    console.log(label + "=" + v);
  } catch (e) {
    console.log(label + ":throw:" + e.name);
  }
};

process.env.OAM_T_NUM = 42;
console.log("num=" + JSON.stringify(process.env.OAM_T_NUM));

process.env.OAM_T_BOOL = true;
console.log("bool=" + JSON.stringify(process.env.OAM_T_BOOL));

process.env.OAM_T_UNDEF = undefined;
console.log("undef=" + JSON.stringify(process.env.OAM_T_UNDEF));

console.log("has=" + ("OAM_T_NUM" in process.env));
console.log("del=" + (delete process.env.OAM_T_NUM));
console.log("has-after=" + ("OAM_T_NUM" in process.env));
console.log("del-missing=" + (delete process.env.OAM_T_MISSING));

const sym = Symbol("s");
console.log("sym-get=" + process.env[sym]);
console.log("sym-in=" + (sym in process.env));
console.log("sym-del=" + (delete process.env[sym]));
tag("sym-set", () => { process.env[sym] = 1; return "nothrow"; });
tag("sym-val", () => { process.env.OAM_T_SYMVAL = sym; return "nothrow"; });

tag("defprop-partial", () => {
  Object.defineProperty(process.env, "OAM_T_DP", { value: "x" });
  return "nothrow";
});
tag("defprop-accessor", () => {
  Object.defineProperty(process.env, "OAM_T_DP", { get() { return "x"; } });
  return "nothrow";
});
Object.defineProperty(process.env, "OAM_T_DP", {
  value: "ok",
  configurable: true,
  writable: true,
  enumerable: true,
});
console.log("defprop-ok=" + JSON.stringify(process.env.OAM_T_DP));
