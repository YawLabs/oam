#!/usr/bin/env python3
"""Batch 14: crypto.createECDH (P-256, P-384) with full ECDH class."""

import sys

path = "js/node_compat.js"
with open(path, "r", encoding="utf-8") as f:
    lines = f.readlines()

original_len = len(lines)

# ======================================================================
# 1. Add ECDH class before the publicEncrypt function
# ======================================================================

# Find "    function publicEncrypt(" which we added in batch 13
target_idx = None
for i in range(len(lines)):
    if "    function publicEncrypt(" in lines[i]:
        target_idx = i
        break

if target_idx is None:
    print("ERROR: Could not find publicEncrypt function")
    sys.exit(1)

print(f"Found publicEncrypt at line {target_idx + 1}")

ecdh_class = [
    "\n",
    "    class ECDH {\n",
    "      constructor(curve) {\n",
    "        this._curve = curve;\n",
    "        this._publicKey = null;\n",
    "        this._privateKey = null;\n",
    "      }\n",
    "      generateKeys(encoding, format) {\n",
    "        var result = natives.cryptoEcdhGenerateKeys(this._curve);\n",
    "        this._publicKey = result.publicKey;\n",
    "        this._privateKey = result.privateKey;\n",
    "        return this.getPublicKey(encoding, format);\n",
    "      }\n",
    "      computeSecret(otherPublicKey, inputEncoding, outputEncoding) {\n",
    "        if (!this._privateKey) throw new Error(\"ECDH: keys have not been generated\");\n",
    "        var otherKey = typeof otherPublicKey === \"string\"\n",
    "          ? BufferCtor.from(otherPublicKey, inputEncoding || \"utf8\")\n",
    "          : otherPublicKey;\n",
    "        var secret = natives.cryptoEcdhComputeSecret(this._curve, new Uint8Array(this._privateKey), new Uint8Array(otherKey));\n",
    "        var buf = BufferCtor.from(secret);\n",
    "        return outputEncoding ? buf.toString(outputEncoding) : buf;\n",
    "      }\n",
    "      getPublicKey(encoding, format) {\n",
    "        if (!this._publicKey) throw new Error(\"ECDH: keys have not been generated\");\n",
    "        var buf = BufferCtor.from(this._publicKey);\n",
    "        if (format === \"compressed\") {\n",
    "          var len = (buf.length - 1) / 2;\n",
    "          var x = buf.subarray(1, 1 + len);\n",
    "          var prefix = (buf[buf.length - 1] & 1) ? 0x03 : 0x02;\n",
    "          var out = BufferCtor.alloc(1 + len);\n",
    "          out[0] = prefix;\n",
    "          x.copy(out, 1);\n",
    "          buf = out;\n",
    "        }\n",
    "        return encoding ? buf.toString(encoding) : buf;\n",
    "      }\n",
    "      getPrivateKey(encoding) {\n",
    "        if (!this._privateKey) throw new Error(\"ECDH: keys have not been generated\");\n",
    "        var buf = BufferCtor.from(this._privateKey);\n",
    "        return encoding ? buf.toString(encoding) : buf;\n",
    "      }\n",
    "      setPrivateKey(key, encoding) {\n",
    "        this._privateKey = typeof key === \"string\"\n",
    "          ? new Uint8Array(BufferCtor.from(key, encoding || \"utf8\"))\n",
    "          : new Uint8Array(key);\n",
    "        this._publicKey = natives.cryptoEcdhGetPublicKey(this._curve, this._privateKey);\n",
    "      }\n",
    "      setPublicKey(key, encoding) {\n",
    "        this._publicKey = typeof key === \"string\"\n",
    "          ? new Uint8Array(BufferCtor.from(key, encoding || \"utf8\"))\n",
    "          : new Uint8Array(key);\n",
    "      }\n",
    "    }\n",
    "\n",
    "    function createECDH(curveName) {\n",
    "      return new ECDH(curveName);\n",
    "    }\n",
    "\n",
]

lines[target_idx:target_idx] = ecdh_class
offset1 = len(ecdh_class)
print(f"  Inserted ECDH class + createECDH ({offset1} lines)")

# ======================================================================
# 2. Add createECDH + ECDH to the return block
# ======================================================================

# Find "      publicEncrypt," in the return block (after offset)
for i in range(target_idx + offset1, len(lines)):
    if "      publicEncrypt," in lines[i]:
        insert_lines = [
            "      createECDH,\n",
            "      ECDH,\n",
        ]
        for j, line in enumerate(insert_lines):
            lines.insert(i + j, line)
        print(f"  Added createECDH/ECDH to return block at line {i + 1}")
        break

with open(path, "w", encoding="utf-8") as f:
    f.writelines(lines)

print(f"\nDone. {original_len} -> {len(lines)} lines (+{len(lines) - original_len})")
