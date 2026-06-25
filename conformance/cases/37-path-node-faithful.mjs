// path.win32 / path.posix parity with Node v22's lib/path.js algorithms.
// Guards the makePathModule port: win32 device roots (\\.\, \\?\), reserved
// device names (CON:, COM1:), drive-relative normalize (C:.), posix trailing
// "./" preservation, win32 relative's Unicode-casing branch, and
// toNamespacedPath returning the resolved (slash-normalized) path.
import path from "node:path";

const w = path.win32;
const p = path.posix;
const B = "\\";

const show = (label, value) => console.log(label + " " + JSON.stringify(value));

// --- win32 device roots: never get a spurious trailing separator ---
show("w.normalize(dev)", w.normalize(B + B + ".\\foo"));
show("w.normalize(dev-trail)", w.normalize(B + B + ".\\foo\\"));
show("w.resolve(physdrv)", w.resolve(B + B + ".\\PHYSICALDRIVE0"));
show("w.resolve(qdrv)", w.resolve(B + B + "?\\PHYSICALDRIVE0"));

// --- win32 reserved device names trip the .\ guard (CVE-2024-36139 family) ---
show("w.normalize(CON)", w.normalize("CON:foo"));
show("w.normalize(COM1)", w.normalize("COM1:bar"));
show("w.normalize(COM0)", w.normalize("COM0:bar")); // NOT reserved
show("w.join(CON)", w.join("a", "CON:..\\..\\b"));

// --- drive-relative + trailing-./ + UNC ---
show("w.normalize(C:.)", w.normalize("C:"));
show("p.normalize(./)", p.normalize("./"));
show("p.join(./)", p.join(".", "./"));
show("w.normalize(UNC)", w.normalize(B + B + "server\\share"));

// --- relative: Unicode-casing branch (Turkish dotted-I) ---
show("w.relative(turk)", w.relative("c:\\a\\İ", "c:\\a\\İ\\test.txt"));
show("w.relative(unc)", w.relative(B + B + "foo\\baz-quux", B + B + "foo\\baz"));

// --- toNamespacedPath normalizes forward slashes in the body ---
show("w.toNamespaced(mix)", w.toNamespacedPath(B + B + "?\\c:\\Windows/System"));

// --- posix basename/extname edge: trailing ".." has no extension ---
show("p.extname(..)", p.extname("/path/to/.."));
show("p.basename(..)", p.basename(".."));
