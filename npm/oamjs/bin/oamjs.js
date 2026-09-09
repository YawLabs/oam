#!/usr/bin/env node
'use strict';

const { spawn } = require('node:child_process');
const { resolveBinary } = require('../lib/resolve.js');
const { forwardSignals, exitLike } = require('../lib/signals.js');

// Errors are written and the exit code is SET, never process.exit()'d: stderr
// is an async pipe when the caller redirected it, and exiting on the next tick
// truncates the message that explains the failure.
function die(message) {
  process.stderr.write(`oamjs: error: ${message}\n`);
  process.exitCode = 1;
}

function main() {
  let binary;
  try {
    binary = resolveBinary();
  } catch (err) {
    return die(err.message);
  }

  // stdio: 'inherit' hands the child our actual descriptors, so an MCP server
  // reads and writes the host's pipes directly. Anything else -- 'pipe' plus a
  // relay, even a byte-exact one -- inserts a buffer boundary into a protocol
  // that frames on the stream, and turns one blocking write into two.
  const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit' });

  const stopForwarding = forwardSignals(child);

  child.on('error', (err) => {
    stopForwarding();
    die(`could not execute ${binary}: ${err.message}`);
  });

  child.on('exit', (code, signal) => {
    // Before exitLike: the re-raise it performs relies on the default signal
    // disposition being back in place.
    stopForwarding();
    exitLike(code, signal);
  });
}

main();
