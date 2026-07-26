// Vendored from Node.js v22.22.2 (commit 2645dc73720b1b4f27c49f395d3c66025ce126cc)
// Source: https://raw.githubusercontent.com/nodejs/node/v22.22.2/lib/internal/streams/add-abort-signal.js
// Retrieved: 2026-07-25. Local modifications: none (loaded via oam's
// build-time define() wrapper -- see docs/design/streams-port.md section 3).
// License: MIT -- see the "Node.js" entry in THIRD_PARTY_LICENSES.md and any
// retained upstream header below. UPSTREAM (same dir) records the pristine
// sha256 of the body below this banner; re-vendor procedure lives there.

'use strict';

const {
  SymbolDispose,
} = require('internal/util');

const {
  AbortError,
  codes: {
    ERR_INVALID_ARG_TYPE,
  },
} = require('internal/errors');

const {
  isNodeStream,
  isWebStream,
  kControllerErrorFunction,
} = require('internal/streams/utils');

const eos = require('internal/streams/end-of-stream');
let addAbortListener;

// This method is inlined here for readable-stream
// It also does not allow for signal to not exist on the stream
// https://github.com/nodejs/node/pull/36061#discussion_r533718029
const validateAbortSignal = (signal, name) => {
  if (typeof signal !== 'object' ||
       !('aborted' in signal)) {
    throw new ERR_INVALID_ARG_TYPE(name, 'AbortSignal', signal);
  }
};

module.exports.addAbortSignal = function addAbortSignal(signal, stream) {
  validateAbortSignal(signal, 'signal');
  if (!isNodeStream(stream) && !isWebStream(stream)) {
    throw new ERR_INVALID_ARG_TYPE('stream', ['ReadableStream', 'WritableStream', 'Stream'], stream);
  }
  return module.exports.addAbortSignalNoValidate(signal, stream);
};

module.exports.addAbortSignalNoValidate = function(signal, stream) {
  if (typeof signal !== 'object' || !('aborted' in signal)) {
    return stream;
  }
  const onAbort = isNodeStream(stream) ?
    () => {
      stream.destroy(new AbortError(undefined, { cause: signal.reason }));
    } :
    () => {
      stream[kControllerErrorFunction](new AbortError(undefined, { cause: signal.reason }));
    };
  if (signal.aborted) {
    onAbort();
  } else {
    addAbortListener ??= require('internal/events/abort_listener').addAbortListener;
    const disposable = addAbortListener(signal, onAbort);
    eos(stream, disposable[SymbolDispose]);
  }
  return stream;
};
