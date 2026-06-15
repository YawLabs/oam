#!/usr/bin/env python3
"""Batch 18: two final crypto gaps.

1. createPublicKey from private KeyObject
2. generateKeyPairSync JWK output format
"""

import sys

path = "js/node_compat.js"
with open(path, "r", encoding="utf-8") as f:
    lines = f.readlines()

original_len = len(lines)

# ======================================================================
# 1. Fix createPublicKey to handle private KeyObject input
# ======================================================================

target_idx = None
for i in range(len(lines)):
    if 'createPublicKey from private KeyObject not yet supported' in lines[i]:
        target_idx = i
        break

if target_idx is None:
    print("ERROR: Could not find createPublicKey private KeyObject throw")
    sys.exit(1)

print(f"Found createPublicKey throw at line {target_idx + 1}")

# Replace the throw with actual extraction
lines[target_idx] = '          pem = natives.cryptoExtractPublicPem(input._pem);\n'

# But we also need to handle the flow -- after setting pem, we need to skip
# the rest of the if/else chain. Let me check the structure.
# The current code:
#   if (input instanceof KeyObject && input.type === "private") {
#     throw ... <-- we're replacing this
#   }
#   if (input.key instanceof Uint8Array ...
#
# After replacement, pem is set, so the rest of the function proceeds correctly
# since it uses pem to create the KeyObject.

print("  Fixed createPublicKey from private KeyObject")

# ======================================================================
# 2. Fix generateKeyPairSync JWK output format
# ======================================================================

target_idx2 = None
for i in range(len(lines)):
    if "if (format === 'jwk' || privFormat === 'jwk')" in lines[i]:
        target_idx2 = i
        break

if target_idx2 is None:
    print("ERROR: Could not find JWK format check in generateKeyPairSync")
    sys.exit(1)

print(f"Found JWK format check at line {target_idx2 + 1}")

# Find the closing brace of this if block (the throw line + closing brace)
throw_idx = target_idx2 + 1
# The current code is:
#   if (format === 'jwk' || privFormat === 'jwk') {
#     throw new Error('JWK format not yet supported in oam');
#   }

jwk_code = [
    "      if (format === 'jwk') {\n",
    "        var pubComps = natives.cryptoRsaJwkComponents(result.publicKey, false);\n",
    "        pubOut = { kty: 'RSA', n: base64urlEncode(pubComps.n), e: base64urlEncode(pubComps.e) };\n",
    "      }\n",
    "      if (privFormat === 'jwk') {\n",
    "        var privComps = natives.cryptoRsaJwkComponents(result.privateKey, true);\n",
    "        privOut = {\n",
    "          kty: 'RSA',\n",
    "          n: base64urlEncode(privComps.n),\n",
    "          e: base64urlEncode(privComps.e),\n",
    "          d: base64urlEncode(privComps.d),\n",
    "          p: base64urlEncode(privComps.p),\n",
    "          q: base64urlEncode(privComps.q),\n",
    "          dp: base64urlEncode(privComps.dp),\n",
    "          dq: base64urlEncode(privComps.dq),\n",
    "          qi: base64urlEncode(privComps.qi),\n",
    "        };\n",
    "      }\n",
]

# Replace the 3-line if block (if + throw + })
lines[target_idx2:target_idx2 + 3] = jwk_code
offset2 = len(jwk_code) - 3
print(f"  Replaced JWK throw with export logic (+{offset2} lines)")

with open(path, "w", encoding="utf-8") as f:
    f.writelines(lines)

print(f"\nDone. {original_len} -> {len(lines)} lines (+{len(lines) - original_len})")
