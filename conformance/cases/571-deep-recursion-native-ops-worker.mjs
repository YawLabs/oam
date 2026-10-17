// The worker-thread half of #395: unbounded recursion that calls synchronous
// native ops near the bottom of a Worker's JS stack ends in a catchable
// RangeError and the worker reports back, rather than the native stack
// overflowing and killing the whole process. (node gives a worker a 4 MiB
// thread stack.) See case 570 for the probe shape.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { Worker } from 'node:worker_threads';

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'oam-571-'));
fs.writeFileSync(path.join(dir, 'f.txt'), 'x');

const body = `
const fs = require('node:fs');
const path = require('node:path');
const zlib = require('node:zlib');
const { parentPort, workerData: dir } = require('node:worker_threads');
const file = path.join(dir, 'f.txt');
function probe(op) {
  let called = false;
  let depth = 0;
  function recurse() {
    depth++;
    try {
      recurse();
    } catch (err) {
      if (!called) {
        try { op(); called = true; } catch (inner) { if (!(inner instanceof RangeError)) throw inner; }
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
  copyFileSync: () => fs.copyFileSync(file, path.join(dir, 'copy.txt')),
  cpSync: () => fs.cpSync(file, path.join(dir, 'cp.txt')),
  deflateSync: () => zlib.deflateSync('abc'),
};
const out = [];
for (const [name, op] of Object.entries(ops)) out.push(name + ' ' + probe(op));
parentPort.postMessage(out);
`;

const script = path.join(dir, 'worker.cjs');
fs.writeFileSync(script, body);
const worker = new Worker(script, { workerData: dir });
worker.on('message', (lines) => {
  for (const line of lines) console.log('worker', line);
});
worker.on('error', (err) => console.log('worker error', err.name));
worker.on('exit', (code) => {
  console.log('worker exit', code);
  fs.rmSync(dir, { recursive: true, force: true });
});
