// WPT url suite runner: executes the official urltestdata.json constructor
// cases and setters_tests.json setter cases against THIS runtime's URL.
// Prints one JSON document on stdout; the xtask harness parses it.
//
// Usage: oam run wpt_url.mjs --no-check -- <vendor-dir>
import fs from "node:fs";
import path from "node:path";

const vendorDir = process.argv[2];
if (!vendorDir) {
  console.log(JSON.stringify({ error: "usage: wpt_url.mjs <vendor-dir>" }));
  process.exitCode = 2;
} else {
  const constructorCases = JSON.parse(
    fs.readFileSync(path.join(vendorDir, "urltestdata.json"), "utf8"),
  );
  const setterCases = JSON.parse(
    fs.readFileSync(path.join(vendorDir, "setters_tests.json"), "utf8"),
  );

  const FIELDS = [
    "href",
    "protocol",
    "username",
    "password",
    "host",
    "hostname",
    "port",
    "pathname",
    "search",
    "hash",
  ];

  const failures = [];
  let pass = 0;
  let total = 0;

  for (const entry of constructorCases) {
    if (typeof entry === "string") continue; // section comments
    total++;
    let url = null;
    let threw = false;
    try {
      url = new URL(entry.input, entry.base ?? undefined);
    } catch {
      threw = true;
    }
    if (entry.failure === true) {
      if (threw) pass++;
      else {
        failures.push({
          suite: "constructor",
          input: entry.input,
          base: entry.base ?? null,
          expected: "throw",
          got: url.href,
        });
      }
      continue;
    }
    if (threw) {
      failures.push({
        suite: "constructor",
        input: entry.input,
        base: entry.base ?? null,
        expected: entry.href,
        got: "threw",
      });
      continue;
    }
    let ok = true;
    for (const field of FIELDS) {
      if (entry[field] !== undefined && url[field] !== entry[field]) {
        ok = false;
        failures.push({
          suite: "constructor",
          input: entry.input,
          base: entry.base ?? null,
          field,
          expected: entry[field],
          got: url[field],
        });
        break;
      }
    }
    if (ok && entry.origin !== undefined && url.origin !== entry.origin) {
      ok = false;
      failures.push({
        suite: "constructor",
        input: entry.input,
        base: entry.base ?? null,
        field: "origin",
        expected: entry.origin,
        got: url.origin,
      });
    }
    if (ok) pass++;
  }

  let settersPass = 0;
  let settersTotal = 0;
  for (const [property, cases] of Object.entries(setterCases)) {
    if (property === "comment") continue;
    for (const test of cases) {
      settersTotal++;
      let url;
      try {
        url = new URL(test.href);
      } catch {
        failures.push({
          suite: "setters",
          property,
          href: test.href,
          expected: "parse",
          got: "threw",
        });
        continue;
      }
      try {
        url[property] = test.new_value;
      } catch {
        failures.push({
          suite: "setters",
          property,
          href: test.href,
          new_value: test.new_value,
          expected: "no throw",
          got: "threw",
        });
        continue;
      }
      let ok = true;
      for (const [field, expected] of Object.entries(test.expected)) {
        if (url[field] !== expected) {
          ok = false;
          failures.push({
            suite: "setters",
            property,
            href: test.href,
            new_value: test.new_value,
            field,
            expected,
            got: url[field],
          });
          break;
        }
      }
      if (ok) settersPass++;
    }
  }

  console.log(
    JSON.stringify({
      constructor: { pass, total },
      setters: { pass: settersPass, total: settersTotal },
      failureSample: failures.slice(0, 25),
      failureCount: failures.length,
    }),
  );
}
