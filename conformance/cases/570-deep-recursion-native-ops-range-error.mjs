// Unbounded JS recursion that calls a synchronous native op near the bottom
// of the JS stack must end in V8's catchable RangeError -- never in a native
// stack overflow that kills the process (#395). The ops below are the ones
// measured to abort oam on Windows ("thread 'main' has overflowed its stack")
// while the main thread had only a 1 MiB stack reserve under V8's ~984 KiB JS
// limit: copyFileSync, cpSync and zlib.deflateSync on release builds,
// readdirSync and opendirSync too on debug builds. node.exe reserves 8 MiB.
//
// Calling the op at EVERY level makes the case take tens of seconds on
// Windows (thousands of copies), so `probe` recurses with no op until V8
// throws, then, unwinding, calls the op once at the deepest level whose JS
// frames still fit under the limit. That is the same worst case -- native op
// frames starting just under V8's limit -- for a handful of calls.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import zlib from 'node:zlib';

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'oam-570-'));
const file = path.join(dir, 'f.txt');
fs.writeFileSync(file, 'x');

function probe(op) {
  let called = false;
  let depth = 0;
  function recurse() {
    depth++;
    try {
      recurse();
    } catch (err) {
      if (!called) {
        try {
          op();
          called = true;
        } catch (inner) {
          if (!(inner instanceof RangeError)) throw inner;
        }
      }
      throw err;
    }
  }
  try {
    recurse();
    return 'returned';
  } catch (err) {
    return [err instanceof RangeError, err.message, called, depth > 1000].join(' ');
  }
}

const ops = {
  readdirSync: () => fs.readdirSync(dir),
  opendirSync: () => {
    const d = fs.opendirSync(dir);
    d.readSync();
    d.closeSync();
  },
  lstatSync: () => fs.lstatSync(file),
  copyFileSync: () => fs.copyFileSync(file, path.join(dir, 'copy.txt')),
  cpSync: () => fs.cpSync(file, path.join(dir, 'cp.txt')),
  deflateSync: () => zlib.deflateSync('abc'),
};

for (const [name, op] of Object.entries(ops)) {
  console.log(name, probe(op));
}

// The plain shape from the issue: an fs op at every level, for a cheap op.
let depth = 0;
const plain = () => {
  depth++;
  fs.existsSync(file);
  plain();
};
try {
  plain();
} catch (err) {
  console.log('plain existsSync', err instanceof RangeError, err.message, depth > 1000);
}

// The stack is usable again once the RangeError has unwound it.
console.log('after', fs.readFileSync(path.join(dir, 'copy.txt'), 'utf8'));
fs.rmSync(dir, { recursive: true, force: true });
