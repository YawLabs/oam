// 'internal/validators' shim for the vendored Node streams port
// (docs/design/streams-port.md section 5.3): exactly the 5 validators the
// vendored files destructure, with v22 semantics over the shim error codes.
"use strict";

const { codes } = require("internal/errors");

function validateFunction(value, name) {
  if (typeof value !== "function") {
    throw new codes.ERR_INVALID_ARG_TYPE(name, "Function", value);
  }
}

function validateBoolean(value, name) {
  if (typeof value !== "boolean") {
    throw new codes.ERR_INVALID_ARG_TYPE(name, "boolean", value);
  }
}

function validateObject(value, name) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new codes.ERR_INVALID_ARG_TYPE(name, "Object", value);
  }
}

function validateAbortSignal(signal, name) {
  if (
    signal !== undefined &&
    (signal === null ||
      typeof signal !== "object" ||
      !("aborted" in signal))
  ) {
    throw new codes.ERR_INVALID_ARG_TYPE(name, "AbortSignal", signal);
  }
}

function validateInteger(
  value,
  name,
  min = Number.MIN_SAFE_INTEGER,
  max = Number.MAX_SAFE_INTEGER,
) {
  if (typeof value !== "number") {
    throw new codes.ERR_INVALID_ARG_TYPE(name, "number", value);
  }
  if (!Number.isInteger(value)) {
    throw new codes.ERR_OUT_OF_RANGE(name, "an integer", value);
  }
  if (value < min || value > max) {
    throw new codes.ERR_OUT_OF_RANGE(name, `>= ${min} && <= ${max}`, value);
  }
}

module.exports = {
  validateAbortSignal,
  validateBoolean,
  validateFunction,
  validateInteger,
  validateObject,
};
