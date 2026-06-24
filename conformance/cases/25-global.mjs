// Node defines `global` as an alias for the global object: global === globalThis.
// Transpiled CJS deps (node-postgres among them) reference bare `global`; oam
// installed process/Buffer/etc. as globals but never `global`, so those deps
// threw "global is not defined" the moment that code path ran.
console.log("typeof", typeof global);
console.log("is globalThis", global === globalThis);
// bare reference must resolve (this is what the deps actually do).
global.__oam_probe = 42;
console.log("roundtrip", globalThis.__oam_probe);
