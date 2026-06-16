#!/usr/bin/env python3
"""Fix fetch body serialization for Uint8Array/ArrayBuffer bodies.

The fetch wire protocol sends the body as a JSON string field. When the
body is a Uint8Array (e.g. from http.ClientRequest.end()), wellFormed()
converts it via String() which produces comma-separated byte values
("104,101,108,108,111,...") instead of the UTF-8 text. Fix: decode
typed arrays with TextDecoder before serialization.
"""

path = "js/bootstrap.js"
with open(path, "r", encoding="utf-8") as f:
    content = f.read()

old = "body: init.body == null ? null : wellFormed(init.body),"
new = (
    "body: init.body == null\n"
    "        ? null\n"
    "        : init.body instanceof ArrayBuffer || ArrayBuffer.isView(init.body)\n"
    "          ? new TextDecoder().decode(init.body)\n"
    "          : wellFormed(init.body),"
)

assert old in content, f"Pattern not found in {path}"
content = content.replace(old, new, 1)

with open(path, "w", encoding="utf-8") as f:
    f.write(content)

print("Fixed fetch body serialization for typed arrays")
