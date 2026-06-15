#!/usr/bin/env python3
"""Batch 16: crypto.X509Certificate class."""

import sys

path = "js/node_compat.js"
with open(path, "r", encoding="utf-8") as f:
    lines = f.readlines()

original_len = len(lines)

# ======================================================================
# 1. Add X509Certificate class before webcrypto line
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

x509_code = [
    "\n",
    "    // ---- X.509 Certificate ----\n",
    "    class X509Certificate {\n",
    "      constructor(buf) {\n",
    '        if (typeof buf === "string") buf = BufferCtor.from(buf);\n',
    "        else if (!BufferCtor.isBuffer(buf)) buf = BufferCtor.from(buf);\n",
    "        var parsed = natives.cryptoX509Parse(new Uint8Array(buf));\n",
    "        this._subject = parsed.subject;\n",
    "        this._issuer = parsed.issuer;\n",
    "        this._serialNumber = parsed.serialNumber;\n",
    "        this._validFrom = parsed.validFrom;\n",
    "        this._validTo = parsed.validTo;\n",
    "        this._fingerprint = parsed.fingerprint;\n",
    "        this._fingerprint256 = parsed.fingerprint256;\n",
    "        this._ca = parsed.ca;\n",
    '        this._subjectAltName = parsed.subjectAltName || "";\n',
    "        this._keyUsage = parsed.keyUsage || [];\n",
    "        this._raw = BufferCtor.from(parsed.raw);\n",
    "      }\n",
    "      get subject() { return this._subject; }\n",
    "      get issuer() { return this._issuer; }\n",
    "      get serialNumber() { return this._serialNumber; }\n",
    "      get validFrom() { return this._validFrom; }\n",
    "      get validTo() { return this._validTo; }\n",
    "      get fingerprint() { return this._fingerprint; }\n",
    "      get fingerprint256() { return this._fingerprint256; }\n",
    "      get ca() { return this._ca; }\n",
    "      get subjectAltName() { return this._subjectAltName; }\n",
    "      get keyUsage() { return this._keyUsage; }\n",
    "      get raw() { return this._raw; }\n",
    "      toString() {\n",
    '        var b64 = this._raw.toString("base64");\n',
    "        var out = [];\n",
    "        for (var i = 0; i < b64.length; i += 64) out.push(b64.slice(i, i + 64));\n",
    '        return "-----BEGIN CERTIFICATE-----\\n" + out.join("\\n") + "\\n-----END CERTIFICATE-----\\n";\n',
    "      }\n",
    "      toJSON() { return this.toString(); }\n",
    "      toLegacyObject() {\n",
    "        return {\n",
    "          subject: this._subject,\n",
    "          issuer: this._issuer,\n",
    "          serialNumber: this._serialNumber,\n",
    "          valid_from: this._validFrom,\n",
    "          valid_to: this._validTo,\n",
    "          fingerprint: this._fingerprint,\n",
    "          fingerprint256: this._fingerprint256,\n",
    "        };\n",
    "      }\n",
    "    }\n",
    "\n",
]

lines[target_idx:target_idx] = x509_code
offset1 = len(x509_code)
print(f"  Inserted X509Certificate class ({offset1} lines)")

# ======================================================================
# 2. Add X509Certificate to return block (after DiffieHellman)
# ======================================================================

for i in range(target_idx + offset1, len(lines)):
    if "      DiffieHellman," in lines[i]:
        lines.insert(i + 1, "      X509Certificate,\n")
        print(f"  Added X509Certificate export at line {i + 2}")
        break

with open(path, "w", encoding="utf-8") as f:
    f.writelines(lines)

print(f"\nDone. {original_len} -> {len(lines)} lines (+{len(lines) - original_len})")
