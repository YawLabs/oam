#!/usr/bin/env python3
"""Batch 15: crypto.createDiffieHellman / getDiffieHellman (classic DH key exchange)."""

import sys

path = "js/node_compat.js"
with open(path, "r", encoding="utf-8") as f:
    lines = f.readlines()

original_len = len(lines)

# ======================================================================
# 1. Add DiffieHellman class + well-known groups before webcrypto line
# ======================================================================

target_idx = None
for i in range(len(lines)):
    if "const webcrypto = { subtle" in lines[i]:
        target_idx = i
        break

if target_idx is None:
    print("ERROR: Could not find webcrypto line")
    sys.exit(1)

print(f"Found webcrypto at line {target_idx + 1}")

# Well-known DH group primes (RFC 2409 + RFC 3526), all generator 2.
# Hex values verified against OpenSSL dh_group_params.c.
#   modp1  = 768-bit  (RFC 2409 Group 1, 192 hex chars)
#   modp2  = 1024-bit (RFC 2409 Group 2, 256 hex chars)
#   modp5  = 1536-bit (RFC 3526 Group 5, 384 hex chars)
#   modp14 = 2048-bit (RFC 3526 Group 14, 512 hex chars)

MODP1 = (
    "FFFFFFFFFFFFFFFF"
    "C90FDAA22168C234"
    "C4C6628B80DC1CD1"
    "29024E088A67CC74"
    "020BBEA63B139B22"
    "514A08798E3404DD"
    "EF9519B3CD3A431B"
    "302B0A6DF25F1437"
    "4FE1356D6D51C245"
    "E485B576625E7EC6"
    "F44C42E9A637ED6B"
    "FFFFFFFFFFFFFFFF"
)
assert len(MODP1) == 192, f"modp1 length {len(MODP1)} != 192"

MODP2 = (
    "FFFFFFFFFFFFFFFF"
    "C90FDAA22168C234"
    "C4C6628B80DC1CD1"
    "29024E088A67CC74"
    "020BBEA63B139B22"
    "514A08798E3404DD"
    "EF9519B3CD3A431B"
    "302B0A6DF25F1437"
    "4FE1356D6D51C245"
    "E485B576625E7EC6"
    "F44C42E9A637ED6B"
    "0BFF5CB6F406B7ED"
    "EE386BFB5A899FA5"
    "AE9F24117C4B1FE6"
    "49286651ECE65381"
    "FFFFFFFFFFFFFFFF"
)
assert len(MODP2) == 256, f"modp2 length {len(MODP2)} != 256"

MODP5 = (
    "FFFFFFFFFFFFFFFF"
    "C90FDAA22168C234"
    "C4C6628B80DC1CD1"
    "29024E088A67CC74"
    "020BBEA63B139B22"
    "514A08798E3404DD"
    "EF9519B3CD3A431B"
    "302B0A6DF25F1437"
    "4FE1356D6D51C245"
    "E485B576625E7EC6"
    "F44C42E9A637ED6B"
    "0BFF5CB6F406B7ED"
    "EE386BFB5A899FA5"
    "AE9F24117C4B1FE6"
    "49286651ECE45B3D"
    "C2007CB8A163BF05"
    "98DA48361C55D39A"
    "69163FA8FD24CF5F"
    "83655D23DCA3AD96"
    "1C62F356208552BB"
    "9ED529077096966D"
    "670C354E4ABC9804"
    "F1746C08CA237327"
    "FFFFFFFFFFFFFFFF"
)
assert len(MODP5) == 384, f"modp5 length {len(MODP5)} != 384"

MODP14 = (
    "FFFFFFFFFFFFFFFF"
    "C90FDAA22168C234"
    "C4C6628B80DC1CD1"
    "29024E088A67CC74"
    "020BBEA63B139B22"
    "514A08798E3404DD"
    "EF9519B3CD3A431B"
    "302B0A6DF25F1437"
    "4FE1356D6D51C245"
    "E485B576625E7EC6"
    "F44C42E9A637ED6B"
    "0BFF5CB6F406B7ED"
    "EE386BFB5A899FA5"
    "AE9F24117C4B1FE6"
    "49286651ECE45B3D"
    "C2007CB8A163BF05"
    "98DA48361C55D39A"
    "69163FA8FD24CF5F"
    "83655D23DCA3AD96"
    "1C62F356208552BB"
    "9ED529077096966D"
    "670C354E4ABC9804"
    "F1746C08CA18217C"
    "32905E462E36CE3B"
    "E39E772C180E8603"
    "9B2783A2EC07A28F"
    "B5C55DF06F4C52C9"
    "DE2BCBF695581718"
    "3995497CEA956AE5"
    "15D2261898FA0510"
    "15728E5A8AACAA68"
    "FFFFFFFFFFFFFFFF"
)
assert len(MODP14) == 512, f"modp14 length {len(MODP14)} != 512"

dh_code = [
    "\n",
    "    // ---- Diffie-Hellman (classic, non-EC) ----\n",
    "    var DH_GROUPS = {\n",
    f'      modp1: "{MODP1}",\n',
    f'      modp2: "{MODP2}",\n',
    f'      modp5: "{MODP5}",\n',
    f'      modp14: "{MODP14}",\n',
    "    };\n",
    "\n",
    "    class DiffieHellman {\n",
    "      constructor(prime, generator) {\n",
    "        if (typeof prime === \"number\") {\n",
    '          throw new Error("DH prime generation by bit length not yet supported in oam");\n',
    "        }\n",
    "        this._prime = BufferCtor.isBuffer(prime) ? prime : BufferCtor.from(prime);\n",
    "        if (!generator) generator = BufferCtor.from([2]);\n",
    "        else if (typeof generator === \"number\") generator = BufferCtor.from([generator]);\n",
    "        else if (!BufferCtor.isBuffer(generator)) generator = BufferCtor.from(generator);\n",
    "        this._generator = generator;\n",
    "        this._publicKey = null;\n",
    "        this._privateKey = null;\n",
    "      }\n",
    "      generateKeys(encoding) {\n",
    "        var result = natives.cryptoDhGenerateKeys(\n",
    "          new Uint8Array(this._prime),\n",
    "          new Uint8Array(this._generator)\n",
    "        );\n",
    "        this._publicKey = BufferCtor.from(result.publicKey);\n",
    "        this._privateKey = BufferCtor.from(result.privateKey);\n",
    "        return this.getPublicKey(encoding);\n",
    "      }\n",
    "      computeSecret(otherPublicKey, inputEncoding, outputEncoding) {\n",
    '        if (!this._privateKey) throw new Error("DH: keys have not been generated");\n',
    '        var otherKey = typeof otherPublicKey === "string"\n',
    '          ? BufferCtor.from(otherPublicKey, inputEncoding || "hex")\n',
    "          : BufferCtor.from(otherPublicKey);\n",
    "        var secret = natives.cryptoDhComputeSecret(\n",
    "          new Uint8Array(this._prime),\n",
    "          new Uint8Array(this._privateKey),\n",
    "          new Uint8Array(otherKey)\n",
    "        );\n",
    "        var buf = BufferCtor.from(secret);\n",
    "        return outputEncoding ? buf.toString(outputEncoding) : buf;\n",
    "      }\n",
    "      getPrime(encoding) {\n",
    "        var buf = BufferCtor.from(this._prime);\n",
    "        return encoding ? buf.toString(encoding) : buf;\n",
    "      }\n",
    "      getGenerator(encoding) {\n",
    "        var buf = BufferCtor.from(this._generator);\n",
    "        return encoding ? buf.toString(encoding) : buf;\n",
    "      }\n",
    "      getPublicKey(encoding) {\n",
    '        if (!this._publicKey) throw new Error("DH: keys have not been generated");\n',
    "        var buf = BufferCtor.from(this._publicKey);\n",
    "        return encoding ? buf.toString(encoding) : buf;\n",
    "      }\n",
    "      getPrivateKey(encoding) {\n",
    '        if (!this._privateKey) throw new Error("DH: keys have not been generated");\n',
    "        var buf = BufferCtor.from(this._privateKey);\n",
    "        return encoding ? buf.toString(encoding) : buf;\n",
    "      }\n",
    "      setPublicKey(key, encoding) {\n",
    '        this._publicKey = typeof key === "string"\n',
    '          ? BufferCtor.from(key, encoding || "hex")\n',
    "          : BufferCtor.from(key);\n",
    "      }\n",
    "      setPrivateKey(key, encoding) {\n",
    '        this._privateKey = typeof key === "string"\n',
    '          ? BufferCtor.from(key, encoding || "hex")\n',
    "          : BufferCtor.from(key);\n",
    "      }\n",
    "      get verifyError() { return 0; }\n",
    "    }\n",
    "\n",
    "    function createDiffieHellman(primeOrLen, primeEncoding, generator, generatorEncoding) {\n",
    '      if (typeof primeOrLen === "number") {\n',
    '        throw new Error("DH prime generation by bit length not yet supported in oam");\n',
    "      }\n",
    '      var prime = typeof primeOrLen === "string"\n',
    '        ? BufferCtor.from(primeOrLen, primeEncoding || "hex")\n',
    "        : BufferCtor.from(primeOrLen);\n",
    "      var gen;\n",
    "      if (generator === undefined || generator === null) {\n",
    "        gen = BufferCtor.from([2]);\n",
    '      } else if (typeof generator === "number") {\n',
    "        gen = BufferCtor.from([generator]);\n",
    '      } else if (typeof generator === "string") {\n',
    '        gen = BufferCtor.from(generator, generatorEncoding || "hex");\n',
    "      } else {\n",
    "        gen = BufferCtor.from(generator);\n",
    "      }\n",
    "      return new DiffieHellman(prime, gen);\n",
    "    }\n",
    "\n",
    "    function getDiffieHellman(groupName) {\n",
    "      var hex = DH_GROUPS[groupName.toLowerCase()];\n",
    '      if (!hex) throw new Error("Unknown DH group: " + groupName);\n',
    '      return new DiffieHellman(BufferCtor.from(hex, "hex"), BufferCtor.from([2]));\n',
    "    }\n",
    "\n",
]

lines[target_idx:target_idx] = dh_code
offset1 = len(dh_code)
print(f"  Inserted DH class + groups ({offset1} lines)")

# ======================================================================
# 2. Add createDiffieHellman, getDiffieHellman, DiffieHellman to return block
# ======================================================================

for i in range(target_idx + offset1, len(lines)):
    if "      privateDecrypt," in lines[i]:
        insert_lines = [
            "      createDiffieHellman,\n",
            "      getDiffieHellman,\n",
            "      DiffieHellman,\n",
        ]
        for j, line in enumerate(insert_lines):
            lines.insert(i + 1 + j, line)
        print(f"  Added DH exports to return block at line {i + 2}")
        break

with open(path, "w", encoding="utf-8") as f:
    f.writelines(lines)

print(f"\nDone. {original_len} -> {len(lines)} lines (+{len(lines) - original_len})")
