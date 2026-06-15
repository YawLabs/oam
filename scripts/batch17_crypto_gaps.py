#!/usr/bin/env python3
"""Batch 17: five remaining crypto gaps.

1. privateEncrypt / publicDecrypt
2. subtle.importKey JWK
3. createKeyObject from JWK (createPublicKey, createPrivateKey, createSecretKey)
4. createDiffieHellman(primeLength) -- use generatePrime op
5. generatePrime / generatePrimeSync / checkPrimeSync
"""

import sys

path = "js/node_compat.js"
with open(path, "r", encoding="utf-8") as f:
    lines = f.readlines()

original_len = len(lines)

# ======================================================================
# 1. Add ASN.1/JWK helpers + privateEncrypt/publicDecrypt
#    Insert right before the existing publicEncrypt function
# ======================================================================

target_idx = None
for i in range(len(lines)):
    if "function publicEncrypt(keyOrOpts, buffer)" in lines[i]:
        target_idx = i
        break

if target_idx is None:
    print("ERROR: Could not find publicEncrypt function")
    sys.exit(1)

print(f"Found publicEncrypt at line {target_idx + 1}")

helper_code = [
    "    // ---- ASN.1 / JWK helpers ----\n",
    "    function base64urlDecode(str) {\n",
    "      str = str.replace(/-/g, '+').replace(/_/g, '/');\n",
    "      while (str.length % 4 !== 0) str += '=';\n",
    "      return BufferCtor.from(str, 'base64');\n",
    "    }\n",
    "    function base64urlEncode(buf) {\n",
    "      return BufferCtor.from(buf).toString('base64')\n",
    "        .replace(/\\+/g, '-').replace(/\\//g, '_').replace(/=+$/, '');\n",
    "    }\n",
    "    function asn1Length(len) {\n",
    "      if (len < 128) return [len];\n",
    "      var bytes = [];\n",
    "      var tmp = len;\n",
    "      while (tmp > 0) { bytes.unshift(tmp & 0xFF); tmp >>= 8; }\n",
    "      bytes.unshift(0x80 | bytes.length);\n",
    "      return bytes;\n",
    "    }\n",
    "    function asn1Wrap(tag, content) {\n",
    "      var len = asn1Length(content.length);\n",
    "      var out = new Uint8Array(1 + len.length + content.length);\n",
    "      out[0] = tag;\n",
    "      out.set(len, 1);\n",
    "      out.set(content, 1 + len.length);\n",
    "      return out;\n",
    "    }\n",
    "    function asn1Int(bytes) {\n",
    "      if (bytes[0] >= 0x80) {\n",
    "        var padded = new Uint8Array(bytes.length + 1);\n",
    "        padded.set(bytes, 1);\n",
    "        bytes = padded;\n",
    "      }\n",
    "      return asn1Wrap(0x02, bytes);\n",
    "    }\n",
    "    function asn1Seq(parts) {\n",
    "      var totalLen = 0;\n",
    "      for (var i = 0; i < parts.length; i++) totalLen += parts[i].length;\n",
    "      var content = new Uint8Array(totalLen);\n",
    "      var off = 0;\n",
    "      for (var i = 0; i < parts.length; i++) {\n",
    "        content.set(parts[i], off);\n",
    "        off += parts[i].length;\n",
    "      }\n",
    "      return asn1Wrap(0x30, content);\n",
    "    }\n",
    "    var RSA_OID_BYTES = new Uint8Array([0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00]);\n",
    "\n",
    "    function rsaJwkToSpki(jwk) {\n",
    "      var n = new Uint8Array(base64urlDecode(jwk.n));\n",
    "      var e = new Uint8Array(base64urlDecode(jwk.e));\n",
    "      var pubKeySeq = asn1Seq([asn1Int(n), asn1Int(e)]);\n",
    "      var bitStr = asn1Wrap(0x03, (function() {\n",
    "        var bs = new Uint8Array(1 + pubKeySeq.length);\n",
    "        bs[0] = 0x00;\n",
    "        bs.set(pubKeySeq, 1);\n",
    "        return bs;\n",
    "      })());\n",
    "      var totalLen = RSA_OID_BYTES.length + bitStr.length;\n",
    "      var content = new Uint8Array(totalLen);\n",
    "      content.set(RSA_OID_BYTES, 0);\n",
    "      content.set(bitStr, RSA_OID_BYTES.length);\n",
    "      return asn1Wrap(0x30, content);\n",
    "    }\n",
    "\n",
    "    function rsaJwkToPkcs8(jwk) {\n",
    "      var n = new Uint8Array(base64urlDecode(jwk.n));\n",
    "      var e = new Uint8Array(base64urlDecode(jwk.e));\n",
    "      var d = new Uint8Array(base64urlDecode(jwk.d));\n",
    "      var p = new Uint8Array(base64urlDecode(jwk.p));\n",
    "      var q = new Uint8Array(base64urlDecode(jwk.q));\n",
    "      var dp = new Uint8Array(base64urlDecode(jwk.dp));\n",
    "      var dq = new Uint8Array(base64urlDecode(jwk.dq));\n",
    "      var qi = new Uint8Array(base64urlDecode(jwk.qi));\n",
    "      var version = asn1Int(new Uint8Array([0]));\n",
    "      var rsaPriv = asn1Seq([version, asn1Int(n), asn1Int(e), asn1Int(d), asn1Int(p), asn1Int(q), asn1Int(dp), asn1Int(dq), asn1Int(qi)]);\n",
    "      var octetStr = asn1Wrap(0x04, rsaPriv);\n",
    "      var pkcs8Version = asn1Int(new Uint8Array([0]));\n",
    "      return asn1Seq([pkcs8Version, RSA_OID_BYTES, octetStr]);\n",
    "    }\n",
    "\n",
    "    function rsaJwkToPkcs1(jwk) {\n",
    "      var n = new Uint8Array(base64urlDecode(jwk.n));\n",
    "      var e = new Uint8Array(base64urlDecode(jwk.e));\n",
    "      return asn1Seq([asn1Int(n), asn1Int(e)]);\n",
    "    }\n",
    "\n",
]

lines[target_idx:target_idx] = helper_code
offset1 = len(helper_code)
print(f"  Inserted ASN.1/JWK helpers ({offset1} lines)")

# ======================================================================
# 2. Add privateEncrypt / publicDecrypt functions
#    Insert after the existing privateDecrypt function
# ======================================================================

target_idx2 = None
for i in range(len(lines)):
    if "return BufferCtor.from(natives.cryptoPrivateDecrypt(" in lines[i]:
        target_idx2 = i
        break

if target_idx2 is None:
    print("ERROR: Could not find cryptoPrivateDecrypt call")
    sys.exit(1)

# Find the closing brace of privateDecrypt function
close_idx = None
for i in range(target_idx2 + 1, len(lines)):
    if lines[i].strip() == "}":
        close_idx = i
        break

if close_idx is None:
    print("ERROR: Could not find end of privateDecrypt")
    sys.exit(1)

print(f"Found end of privateDecrypt at line {close_idx + 1}")

priv_enc_code = [
    "\n",
    "    function privateEncrypt(keyOrOpts, buffer) {\n",
    "      var key;\n",
    '      if (typeof keyOrOpts === "string") {\n',
    "        key = keyOrOpts;\n",
    "      } else if (ArrayBuffer.isView(keyOrOpts)) {\n",
    "        key = new TextDecoder().decode(keyOrOpts);\n",
    '      } else if (keyOrOpts && typeof keyOrOpts === "object") {\n',
    '        key = typeof keyOrOpts.key === "string" ? keyOrOpts.key : new TextDecoder().decode(keyOrOpts.key);\n',
    "      } else {\n",
    '        throw new TypeError("privateEncrypt: key must be a string, Buffer, or object");\n',
    "      }\n",
    '      var data = typeof buffer === "string" ? BufferCtor.from(buffer) : buffer;\n',
    "      return BufferCtor.from(natives.cryptoPrivateEncrypt(new Uint8Array(data), key));\n",
    "    }\n",
    "\n",
    "    function publicDecrypt(keyOrOpts, buffer) {\n",
    "      var key;\n",
    '      if (typeof keyOrOpts === "string") {\n',
    "        key = keyOrOpts;\n",
    "      } else if (ArrayBuffer.isView(keyOrOpts)) {\n",
    "        key = new TextDecoder().decode(keyOrOpts);\n",
    '      } else if (keyOrOpts && typeof keyOrOpts === "object") {\n',
    '        key = typeof keyOrOpts.key === "string" ? keyOrOpts.key : new TextDecoder().decode(keyOrOpts.key);\n',
    "      } else {\n",
    '        throw new TypeError("publicDecrypt: key must be a string, Buffer, or object");\n',
    "      }\n",
    '      var data = typeof buffer === "string" ? BufferCtor.from(buffer) : buffer;\n',
    "      return BufferCtor.from(natives.cryptoPublicDecrypt(new Uint8Array(data), key));\n",
    "    }\n",
]

lines[close_idx + 1:close_idx + 1] = priv_enc_code
offset2 = len(priv_enc_code)
print(f"  Inserted privateEncrypt/publicDecrypt ({offset2} lines)")

# ======================================================================
# 3. Replace subtle.importKey JWK throw with actual implementation
# ======================================================================

target_idx3 = None
for i in range(len(lines)):
    if 'throw new Error("subtle.importKey: JWK format not yet supported in oam")' in lines[i]:
        target_idx3 = i
        break

if target_idx3 is None:
    print("ERROR: Could not find subtle.importKey JWK throw")
    sys.exit(1)

print(f"Found subtle.importKey JWK throw at line {target_idx3 + 1}")

jwk_import_code = [
    '          var kty = keyData.kty;\n',
    '          if (kty === "oct") {\n',
    '            var raw = new Uint8Array(base64urlDecode(keyData.k));\n',
    '            _importedKeys.set(id, { format: "raw", data: raw, algo: algoObj });\n',
    '            keyType = "secret";\n',
    '          } else if (kty === "RSA") {\n',
    '            if (keyData.d) {\n',
    '              pem = derToPem(rsaJwkToPkcs8(keyData), "PRIVATE KEY");\n',
    '              _importedKeys.set(id, { format: "pkcs8", pem: pem, algo: algoObj });\n',
    '              keyType = "private";\n',
    '            } else {\n',
    '              pem = derToPem(rsaJwkToSpki(keyData), "PUBLIC KEY");\n',
    '              _importedKeys.set(id, { format: "spki", pem: pem, algo: algoObj });\n',
    '              keyType = "public";\n',
    '            }\n',
    '          } else {\n',
    '            throw new Error("subtle.importKey: unsupported JWK kty: " + kty);\n',
    '          }\n',
]

lines[target_idx3:target_idx3 + 1] = jwk_import_code
offset3 = len(jwk_import_code) - 1
print(f"  Replaced subtle.importKey JWK throw (+{offset3} lines)")

# ======================================================================
# 4. Add JWK support to createSecretKey
# ======================================================================

target_idx4 = None
for i in range(len(lines)):
    if "function createSecretKey(key, encoding)" in lines[i]:
        target_idx4 = i
        break

if target_idx4 is None:
    print("ERROR: Could not find createSecretKey")
    sys.exit(1)

print(f"Found createSecretKey at line {target_idx4 + 1}")

# Find the line with "const material = toBytes(key, encoding);"
mat_idx = None
for i in range(target_idx4 + 1, target_idx4 + 10):
    if "const material = toBytes(key, encoding);" in lines[i]:
        mat_idx = i
        break

if mat_idx is None:
    print("ERROR: Could not find toBytes call in createSecretKey")
    sys.exit(1)

jwk_secret_code = [
    '      if (typeof key === "object" && key !== null && key.kty === "oct") {\n',
    '        var raw = new Uint8Array(base64urlDecode(key.k));\n',
    '        return new KeyObject("secret", raw);\n',
    '      }\n',
]

lines[mat_idx:mat_idx] = jwk_secret_code
offset4 = len(jwk_secret_code)
print(f"  Added JWK support to createSecretKey ({offset4} lines)")

# ======================================================================
# 5. Add JWK support to createPrivateKey
# ======================================================================

target_idx5 = None
for i in range(len(lines)):
    if "function createPrivateKey(input)" in lines[i]:
        target_idx5 = i
        break

if target_idx5 is None:
    print("ERROR: Could not find createPrivateKey")
    sys.exit(1)

print(f"Found createPrivateKey at line {target_idx5 + 1}")

# Find the "var pem;" line right after
pem_idx = None
for i in range(target_idx5 + 1, target_idx5 + 5):
    if lines[i].strip() == "var pem;":
        pem_idx = i
        break

if pem_idx is None:
    print("ERROR: Could not find 'var pem;' in createPrivateKey")
    sys.exit(1)

jwk_priv_code = [
    "      var pem;\n",
    '      if (typeof input === "object" && input !== null && input.format === "jwk" && input.key) {\n',
    '        var jwk = input.key;\n',
    '        if (jwk.kty === "RSA") {\n',
    '          pem = derToPem(rsaJwkToPkcs8(jwk), "PRIVATE KEY");\n',
    '        } else {\n',
    '          throw new Error("createPrivateKey: unsupported JWK kty: " + jwk.kty);\n',
    '        }\n',
    '      } else if (typeof input === "string") {\n',
]

# Replace "var pem;" and "if (typeof input === 'string')" lines
# Find the line with 'if (typeof input === "string")'
str_check_idx = None
for i in range(pem_idx + 1, pem_idx + 3):
    if 'typeof input === "string"' in lines[i]:
        str_check_idx = i
        break

if str_check_idx is None:
    print("ERROR: Could not find string check in createPrivateKey")
    sys.exit(1)

lines[pem_idx:str_check_idx + 1] = jwk_priv_code
offset5 = len(jwk_priv_code) - (str_check_idx - pem_idx + 1)
print(f"  Added JWK support to createPrivateKey ({offset5} net lines)")

# ======================================================================
# 6. Add JWK support to createPublicKey
# ======================================================================

target_idx6 = None
for i in range(len(lines)):
    if "function createPublicKey(input)" in lines[i]:
        target_idx6 = i
        break

if target_idx6 is None:
    print("ERROR: Could not find createPublicKey")
    sys.exit(1)

print(f"Found createPublicKey at line {target_idx6 + 1}")

# Find "var pem;" in createPublicKey
pem_idx2 = None
for i in range(target_idx6 + 1, target_idx6 + 5):
    if lines[i].strip() == "var pem;":
        pem_idx2 = i
        break

if pem_idx2 is None:
    print("ERROR: Could not find 'var pem;' in createPublicKey")
    sys.exit(1)

str_check_idx2 = None
for i in range(pem_idx2 + 1, pem_idx2 + 3):
    if 'typeof input === "string"' in lines[i]:
        str_check_idx2 = i
        break

if str_check_idx2 is None:
    print("ERROR: Could not find string check in createPublicKey")
    sys.exit(1)

jwk_pub_code = [
    "      var pem;\n",
    '      if (typeof input === "object" && input !== null && input.format === "jwk" && input.key) {\n',
    '        var jwk = input.key;\n',
    '        if (jwk.kty === "RSA") {\n',
    '          pem = derToPem(rsaJwkToSpki(jwk), "PUBLIC KEY");\n',
    '        } else {\n',
    '          throw new Error("createPublicKey: unsupported JWK kty: " + jwk.kty);\n',
    '        }\n',
    '      } else if (typeof input === "string") {\n',
]

lines[pem_idx2:str_check_idx2 + 1] = jwk_pub_code
offset6 = len(jwk_pub_code) - (str_check_idx2 - pem_idx2 + 1)
print(f"  Added JWK support to createPublicKey ({offset6} net lines)")

# ======================================================================
# 7. Fix createDiffieHellman(primeLength) -- both constructor and factory
# ======================================================================

# Replace DiffieHellman constructor throw
for i in range(len(lines)):
    if 'throw new Error("DH prime generation by bit length not yet supported in oam")' in lines[i]:
        # Check if this is in the constructor (indented more) or factory
        if "class DiffieHellman" in "".join(lines[max(0,i-10):i]):
            # Constructor: generate prime and store it
            lines[i] = '          var primeBytes = BufferCtor.from(natives.cryptoGeneratePrime(prime));\n'
            # Need to also set the prime on this instance
            # Find the next line and add assignment
            lines.insert(i + 1, '          prime = primeBytes;\n')
            print(f"  Fixed DiffieHellman constructor prime generation at line {i + 1}")
            break

# Replace createDiffieHellman factory throw
for i in range(len(lines)):
    if 'throw new Error("DH prime generation by bit length not yet supported in oam")' in lines[i]:
        lines[i] = '        var primeBytes = BufferCtor.from(natives.cryptoGeneratePrime(primeOrLen));\n'
        lines.insert(i + 1, '        return new DiffieHellman(primeBytes, BufferCtor.from([2]));\n')
        print(f"  Fixed createDiffieHellman factory prime generation at line {i + 1}")
        break

# ======================================================================
# 8. Add generatePrime / generatePrimeSync / checkPrimeSync
#    Insert before the crypto return block
# ======================================================================

target_idx8 = None
for i in range(len(lines)):
    if "getCiphers," in lines[i] and "getFips" in lines[i + 1]:
        target_idx8 = i
        break

if target_idx8 is None:
    print("ERROR: Could not find getCiphers in return block")
    sys.exit(1)

print(f"Found return block getCiphers at line {target_idx8 + 1}")

# Find a good insertion point - before the return block starts
# Search backwards from getCiphers for the start of the return object
ret_start_idx = None
for i in range(target_idx8 - 1, target_idx8 - 30, -1):
    if "generateKey:" in lines[i] or "generateKeySync:" in lines[i]:
        # Find the closing of generateKey block
        pass
    if lines[i].strip().startswith("return {"):
        ret_start_idx = i
        break

if ret_start_idx is None:
    # Alternative: search for "return {"
    for i in range(target_idx8 - 20, target_idx8):
        if "return {" in lines[i]:
            ret_start_idx = i
            break

# Insert generatePrime/checkPrime functions before the return block
prime_funcs = [
    "\n",
    "    function generatePrimeSync(size, options) {\n",
    '      var bigint = options && options.bigint;\n',
    "      var bytes = natives.cryptoGeneratePrime(size);\n",
    "      if (bigint) {\n",
    '        var hex = "";\n',
    "        for (var i = 0; i < bytes.length; i++) hex += (\"0\" + bytes[i].toString(16)).slice(-2);\n",
    '        return BigInt("0x" + hex);\n',
    "      }\n",
    "      return BufferCtor.from(bytes);\n",
    "    }\n",
    "\n",
    "    function generatePrime(size, options, callback) {\n",
    '      if (typeof options === "function") { callback = options; options = {}; }\n',
    "      try {\n",
    "        var result = generatePrimeSync(size, options);\n",
    "        if (callback) queueMicrotask(function() { callback(null, result); });\n",
    "        else return result;\n",
    "      } catch (err) {\n",
    "        if (callback) queueMicrotask(function() { callback(err); });\n",
    "        else throw err;\n",
    "      }\n",
    "    }\n",
    "\n",
    "    function checkPrimeSync(candidate, options) {\n",
    "      var buf;\n",
    '      if (typeof candidate === "bigint") {\n',
    "        var hex = candidate.toString(16);\n",
    '        if (hex.length % 2 !== 0) hex = "0" + hex;\n',
    "        buf = BufferCtor.from(hex, 'hex');\n",
    "      } else {\n",
    "        buf = BufferCtor.from(candidate);\n",
    "      }\n",
    "      return natives.cryptoCheckPrime(new Uint8Array(buf));\n",
    "    }\n",
    "\n",
    "    function checkPrime(candidate, options, callback) {\n",
    '      if (typeof options === "function") { callback = options; options = {}; }\n',
    "      try {\n",
    "        var result = checkPrimeSync(candidate, options);\n",
    "        if (callback) queueMicrotask(function() { callback(null, result); });\n",
    "        else return result;\n",
    "      } catch (err) {\n",
    "        if (callback) queueMicrotask(function() { callback(err); });\n",
    "        else throw err;\n",
    "      }\n",
    "    }\n",
    "\n",
]

if ret_start_idx is not None:
    lines[ret_start_idx:ret_start_idx] = prime_funcs
    offset8 = len(prime_funcs)
    print(f"  Inserted generatePrime/checkPrime functions ({offset8} lines)")
else:
    print("WARNING: Could not find return block start, inserting before getCiphers")
    lines[target_idx8:target_idx8] = prime_funcs
    offset8 = len(prime_funcs)
    print(f"  Inserted generatePrime/checkPrime functions ({offset8} lines)")

# ======================================================================
# 9. Add new exports to the return block
# ======================================================================

# Add privateEncrypt, publicDecrypt exports
for i in range(len(lines)):
    if "      publicEncrypt," in lines[i]:
        lines.insert(i + 1, "      privateEncrypt,\n")
        print(f"  Added privateEncrypt export after line {i + 1}")
        break

for i in range(len(lines)):
    if "      privateDecrypt," in lines[i]:
        lines.insert(i + 1, "      publicDecrypt,\n")
        print(f"  Added publicDecrypt export after line {i + 1}")
        break

# Add generatePrime/checkPrime exports
for i in range(len(lines)):
    if "      X509Certificate," in lines[i]:
        exports_to_add = [
            "      generatePrime,\n",
            "      generatePrimeSync,\n",
            "      checkPrime,\n",
            "      checkPrimeSync,\n",
        ]
        for j, ex in enumerate(exports_to_add):
            lines.insert(i + 1 + j, ex)
        print(f"  Added prime exports after X509Certificate at line {i + 1}")
        break

with open(path, "w", encoding="utf-8") as f:
    f.writelines(lines)

print(f"\nDone. {original_len} -> {len(lines)} lines (+{len(lines) - original_len})")
