// Vendored from Node.js v26.13.1 (commit e9c277643b0249c50f6e6de5413be8a88ea11b6a)
// Source: https://raw.githubusercontent.com/nodejs/node/v26.13.1/lib/stream/promises.js
// Retrieved: 2026-11-22. Local modifications: none (loaded via oam's
// build-time define() wrapper -- see docs/design/streams-port.md section 3).
// License: MIT -- see the "Node.js" entry in THIRD_PARTY_LICENSES.md and any
// retained upstream header below. UPSTREAM (same dir) records the pristine
// sha256 of the body below this banner; re-vendor procedure lives there.

'use strict';

const {
  ArrayIsArray,
  ArrayPrototypePop,
  Promise,
} = primordials;

const {
  isIterable,
  isNodeStream,
  isWebStream,
} = require('internal/streams/utils');

const { pipelineImpl: pl } = require('internal/streams/pipeline');
const { finished } = require('internal/streams/end-of-stream');

require('stream');

function pipeline(...streams) {
  return new Promise((resolve, reject) => {
    let signal;
    let end;
    const lastArg = streams[streams.length - 1];
    if (lastArg && typeof lastArg === 'object' &&
        !isNodeStream(lastArg) && !isIterable(lastArg) && !isWebStream(lastArg)) {
      const options = ArrayPrototypePop(streams);
      signal = options.signal;
      end = options.end;
    }

    let lastStream = streams[streams.length - 1];
    if (streams.length === 1 && ArrayIsArray(streams[0])) {
      lastStream = streams[0][streams[0].length - 1];
    }

    const stream = pl(streams, (err, value) => {
      if (err) {
        reject(err);
      } else {
        resolve(value);
      }
    }, { signal, end });

    if (typeof lastStream === 'function' && stream.readable) {
      stream.resume();
    }
  });
}

module.exports = {
  finished,
  pipeline,
};
