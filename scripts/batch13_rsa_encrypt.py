#!/usr/bin/env python3
"""Batch 13: crypto.publicEncrypt/privateDecrypt + RSA keygen support."""

import sys

path = "js/node_compat.js"
with open(path, "r", encoding="utf-8") as f:
    lines = f.readlines()

original_len = len(lines)

# ======================================================================
# 1. Fix generateKeyPairSync to pass modulusLength for RSA
# ======================================================================

for i in range(len(lines)):
    if "const result = natives.cryptoGenerateKeyPair(type);" in lines[i]:
        lines[i] = lines[i].replace(
            "natives.cryptoGenerateKeyPair(type)",
            "natives.cryptoGenerateKeyPair(type, (options && options.modulusLength) || 0)"
        )
        print(f"  Patched generateKeyPairSync at line {i + 1}")
        break

# ======================================================================
# 2. Add publicEncrypt / privateDecrypt functions before the return block
# ======================================================================

# Find "    const webcrypto = { subtle" which is right before Certificate class
webcrypto_idx = None
for i in range(len(lines)):
    if "const webcrypto = { subtle" in lines[i]:
        webcrypto_idx = i
        break

if webcrypto_idx is None:
    print("ERROR: Could not find webcrypto line")
    sys.exit(1)

print(f"Found webcrypto at line {webcrypto_idx + 1}")

rsa_funcs = [
    "\n",
    "    function publicEncrypt(keyOrOpts, buffer) {\n",
    "      var key, padding = 4, oaepHash = \"sha1\";\n",
    "      if (typeof keyOrOpts === \"string\") {\n",
    "        key = keyOrOpts;\n",
    "      } else if (ArrayBuffer.isView(keyOrOpts)) {\n",
    "        key = new TextDecoder().decode(keyOrOpts);\n",
    "      } else if (keyOrOpts && typeof keyOrOpts === \"object\") {\n",
    "        key = typeof keyOrOpts.key === \"string\" ? keyOrOpts.key : new TextDecoder().decode(keyOrOpts.key);\n",
    "        if (keyOrOpts.padding !== undefined) padding = keyOrOpts.padding;\n",
    "        if (keyOrOpts.oaepHash) oaepHash = keyOrOpts.oaepHash;\n",
    "      } else {\n",
    "        throw new TypeError(\"publicEncrypt: key must be a string, Buffer, or object\");\n",
    "      }\n",
    "      var paddingName = padding === 1 ? \"pkcs1\" : \"oaep\";\n",
    "      var data = typeof buffer === \"string\" ? BufferCtor.from(buffer) : buffer;\n",
    "      return BufferCtor.from(natives.cryptoPublicEncrypt(new Uint8Array(data), key, paddingName, oaepHash));\n",
    "    }\n",
    "\n",
    "    function privateDecrypt(keyOrOpts, buffer) {\n",
    "      var key, padding = 4, oaepHash = \"sha1\";\n",
    "      if (typeof keyOrOpts === \"string\") {\n",
    "        key = keyOrOpts;\n",
    "      } else if (ArrayBuffer.isView(keyOrOpts)) {\n",
    "        key = new TextDecoder().decode(keyOrOpts);\n",
    "      } else if (keyOrOpts && typeof keyOrOpts === \"object\") {\n",
    "        key = typeof keyOrOpts.key === \"string\" ? keyOrOpts.key : new TextDecoder().decode(keyOrOpts.key);\n",
    "        if (keyOrOpts.padding !== undefined) padding = keyOrOpts.padding;\n",
    "        if (keyOrOpts.oaepHash) oaepHash = keyOrOpts.oaepHash;\n",
    "      } else {\n",
    "        throw new TypeError(\"privateDecrypt: key must be a string, Buffer, or object\");\n",
    "      }\n",
    "      var paddingName = padding === 1 ? \"pkcs1\" : \"oaep\";\n",
    "      var data = typeof buffer === \"string\" ? BufferCtor.from(buffer) : buffer;\n",
    "      return BufferCtor.from(natives.cryptoPrivateDecrypt(new Uint8Array(data), key, paddingName, oaepHash));\n",
    "    }\n",
    "\n",
]

lines[webcrypto_idx:webcrypto_idx] = rsa_funcs
offset1 = len(rsa_funcs)
print(f"  Inserted publicEncrypt/privateDecrypt ({offset1} lines)")

# ======================================================================
# 3. Add publicEncrypt/privateDecrypt to the return block
# ======================================================================

# Find "      createSign," in the return block (after offset)
for i in range(webcrypto_idx + offset1, len(lines)):
    if "      createSign," in lines[i]:
        insert_lines = [
            "      publicEncrypt,\n",
            "      privateDecrypt,\n",
        ]
        for j, line in enumerate(insert_lines):
            lines.insert(i + j, line)
        print(f"  Added publicEncrypt/privateDecrypt to return block at line {i + 1}")
        break

with open(path, "w", encoding="utf-8") as f:
    f.writelines(lines)

print(f"\nDone. {original_len} -> {len(lines)} lines (+{len(lines) - original_len})")
