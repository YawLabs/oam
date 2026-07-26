// Vendored from Node.js v22.22.2 (commit 2645dc73720b1b4f27c49f395d3c66025ce126cc)
// Source: https://raw.githubusercontent.com/nodejs/node/v22.22.2/lib/internal/streams/state.js
// Retrieved: 2026-07-25. Local modifications: none (loaded via oam's
// build-time define() wrapper -- see docs/design/streams-port.md section 3).
// License: MIT -- see the "Node.js" entry in THIRD_PARTY_LICENSES.md and any
// retained upstream header below. UPSTREAM (same dir) records the pristine
// sha256 of the body below this banner; re-vendor procedure lives there.

'use strict';

const {
  MathFloor,
  NumberIsInteger,
} = primordials;
const { validateInteger } = require('internal/validators');

const { ERR_INVALID_ARG_VALUE } = require('internal/errors').codes;

// TODO (fix): For some reason Windows CI fails with bigger hwm.
let defaultHighWaterMarkBytes = process.platform === 'win32' ? 16 * 1024 : 64 * 1024;
let defaultHighWaterMarkObjectMode = 16;

function highWaterMarkFrom(options, isDuplex, duplexKey) {
  return options.highWaterMark != null ? options.highWaterMark :
    isDuplex ? options[duplexKey] : null;
}

function getDefaultHighWaterMark(objectMode) {
  return objectMode ? defaultHighWaterMarkObjectMode : defaultHighWaterMarkBytes;
}

function setDefaultHighWaterMark(objectMode, value) {
  validateInteger(value, 'value', 0);
  if (objectMode) {
    defaultHighWaterMarkObjectMode = value;
  } else {
    defaultHighWaterMarkBytes = value;
  }
}

function getHighWaterMark(state, options, duplexKey, isDuplex) {
  const hwm = highWaterMarkFrom(options, isDuplex, duplexKey);
  if (hwm != null) {
    if (!NumberIsInteger(hwm) || hwm < 0) {
      const name = isDuplex ? `options.${duplexKey}` : 'options.highWaterMark';
      throw new ERR_INVALID_ARG_VALUE(name, hwm);
    }
    return MathFloor(hwm);
  }

  // Default value
  return getDefaultHighWaterMark(state.objectMode);
}

module.exports = {
  getHighWaterMark,
  getDefaultHighWaterMark,
  setDefaultHighWaterMark,
};
