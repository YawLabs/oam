"""
Fix 1: Make Sign/Verify extend stream.Transform so they work as streaming classes.

Libraries like jsonwebtoken pipe data into Sign/Verify via .write()/_write().
The existing classes have the right core logic but aren't stream-compatible.

This script:
1. Adds a `const stream = registry.get("stream")` import at top of crypto factory
2. Makes Sign/Verify extend stream.Transform (constructor calls super(), adds _transform)
"""

FILE = r"C:\Users\jeff\yaw\oam_js_runtime\oam\.claude\worktrees\agent-a9b6eca4fb275c11c\js\node_compat.js"

with open(FILE, "r", encoding="utf-8") as f:
    content = f.read()

# ---------- Step 1: Add stream import near top of crypto factory ----------
# Find the line: "const BufferCtor = globalThis.Buffer;" inside crypto factory
# and add the stream import right after it
old_crypto_top = '''  registry.factories.crypto = (natives) => {
    const BufferCtor = globalThis.Buffer;'''

new_crypto_top = '''  registry.factories.crypto = (natives) => {
    const BufferCtor = globalThis.Buffer;
    const stream = registry.get("stream");'''

if old_crypto_top not in content:
    print("ERROR: Could not find crypto factory top")
    exit(1)

content = content.replace(old_crypto_top, new_crypto_top, 1)
print("Added stream import to crypto factory")

# ---------- Step 2: Replace Sign class to extend stream.Transform ----------
old_sign = '''    class Sign {
      constructor(algorithm) {
        this._algorithm = algorithm;
        this._data = [];
      }
      update(data, inputEncoding) {
        this._data.push(toBytes(data, inputEncoding));
        return this;
      }
      sign(key, outputEncoding) {
        var pem, keyType;
        if (key instanceof KeyObject) {
          pem = key._pem || new TextDecoder().decode(key._material);
          keyType = key._keyType || "rsa";
        } else if (typeof key === "object" && key !== null && !(key instanceof Uint8Array)) {
          pem = typeof key.key === "string" ? key.key : new TextDecoder().decode(key.key);
          keyType = detectKeyType(pem);
        } else {
          pem = typeof key === "string" ? key : new TextDecoder().decode(key);
          keyType = detectKeyType(pem);
        }
        var total = 0;
        for (var i = 0; i < this._data.length; i++) total += this._data[i].length;
        var merged = new Uint8Array(total);
        var off = 0;
        for (var i = 0; i < this._data.length; i++) { merged.set(this._data[i], off); off += this._data[i].length; }
        var padding = 1;
        var saltLength = 32;
        if (key instanceof KeyObject) {
          // KeyObject does not carry padding; default PKCS1
        } else if (typeof key === "object" && key !== null && !(key instanceof Uint8Array)) {
          if (key.padding !== undefined) padding = key.padding;
          if (key.saltLength !== undefined) saltLength = key.saltLength;
        }
        var sig;
        if (padding === 6 && keyType === "rsa") {
          sig = asBuffer(natives.cryptoSignPss(this._algorithm, merged, pem, saltLength));
        } else {
          sig = asBuffer(natives.cryptoSign(this._algorithm, merged, pem, keyType));
        }
        return outputEncoding ? sig.toString(outputEncoding) : sig;
      }
    }'''

new_sign = '''    class Sign extends stream.Transform {
      constructor(algorithm) {
        super();
        this._algorithm = algorithm;
        this._data = [];
      }
      update(data, inputEncoding) {
        this._data.push(toBytes(data, inputEncoding));
        return this;
      }
      sign(key, outputEncoding) {
        var pem, keyType;
        if (key instanceof KeyObject) {
          pem = key._pem || new TextDecoder().decode(key._material);
          keyType = key._keyType || "rsa";
        } else if (typeof key === "object" && key !== null && !(key instanceof Uint8Array)) {
          pem = typeof key.key === "string" ? key.key : new TextDecoder().decode(key.key);
          keyType = detectKeyType(pem);
        } else {
          pem = typeof key === "string" ? key : new TextDecoder().decode(key);
          keyType = detectKeyType(pem);
        }
        var total = 0;
        for (var i = 0; i < this._data.length; i++) total += this._data[i].length;
        var merged = new Uint8Array(total);
        var off = 0;
        for (var i = 0; i < this._data.length; i++) { merged.set(this._data[i], off); off += this._data[i].length; }
        var padding = 1;
        var saltLength = 32;
        if (key instanceof KeyObject) {
          // KeyObject does not carry padding; default PKCS1
        } else if (typeof key === "object" && key !== null && !(key instanceof Uint8Array)) {
          if (key.padding !== undefined) padding = key.padding;
          if (key.saltLength !== undefined) saltLength = key.saltLength;
        }
        var sig;
        if (padding === 6 && keyType === "rsa") {
          sig = asBuffer(natives.cryptoSignPss(this._algorithm, merged, pem, saltLength));
        } else {
          sig = asBuffer(natives.cryptoSign(this._algorithm, merged, pem, keyType));
        }
        return outputEncoding ? sig.toString(outputEncoding) : sig;
      }
      _transform(chunk, encoding, callback) {
        this.update(chunk, encoding);
        callback();
      }
    }'''

if old_sign not in content:
    print("ERROR: Could not find old Sign class")
    exit(1)

content = content.replace(old_sign, new_sign, 1)
print("Replaced Sign class to extend stream.Transform with _transform()")

# ---------- Step 3: Replace Verify class similarly ----------
old_verify = '''    class Verify {
      constructor(algorithm) {
        this._algorithm = algorithm;
        this._data = [];
      }
      update(data, inputEncoding) {
        this._data.push(toBytes(data, inputEncoding));
        return this;
      }
      verify(key, signature, signatureEncoding) {
        var pem, keyType;
        if (key instanceof KeyObject) {
          pem = key._pem || new TextDecoder().decode(key._material);
          keyType = key._keyType || "rsa";
        } else if (typeof key === "object" && key !== null && !(key instanceof Uint8Array)) {
          pem = typeof key.key === "string" ? key.key : new TextDecoder().decode(key.key);
          keyType = detectKeyType(pem);
        } else {
          pem = typeof key === "string" ? key : new TextDecoder().decode(key);
          keyType = detectKeyType(pem);
        }
        var total = 0;
        for (var i = 0; i < this._data.length; i++) total += this._data[i].length;
        var merged = new Uint8Array(total);
        var off = 0;
        for (var i = 0; i < this._data.length; i++) { merged.set(this._data[i], off); off += this._data[i].length; }
        var sigBuf = typeof signature === "string"
          ? BufferCtor.from(signature, signatureEncoding || "base64")
          : toBytes(signature);
        var padding = 1;
        var saltLength = 32;
        if (key instanceof KeyObject) {
          // KeyObject does not carry padding; default PKCS1
        } else if (typeof key === "object" && key !== null && !(key instanceof Uint8Array)) {
          if (key.padding !== undefined) padding = key.padding;
          if (key.saltLength !== undefined) saltLength = key.saltLength;
        }
        if (padding === 6 && keyType === "rsa") {
          return natives.cryptoVerifyPss(this._algorithm, merged, pem, sigBuf, saltLength);
        }
        return natives.cryptoVerify(this._algorithm, merged, pem, sigBuf, keyType);
      }
    }'''

new_verify = '''    class Verify extends stream.Transform {
      constructor(algorithm) {
        super();
        this._algorithm = algorithm;
        this._data = [];
      }
      update(data, inputEncoding) {
        this._data.push(toBytes(data, inputEncoding));
        return this;
      }
      verify(key, signature, signatureEncoding) {
        var pem, keyType;
        if (key instanceof KeyObject) {
          pem = key._pem || new TextDecoder().decode(key._material);
          keyType = key._keyType || "rsa";
        } else if (typeof key === "object" && key !== null && !(key instanceof Uint8Array)) {
          pem = typeof key.key === "string" ? key.key : new TextDecoder().decode(key.key);
          keyType = detectKeyType(pem);
        } else {
          pem = typeof key === "string" ? key : new TextDecoder().decode(key);
          keyType = detectKeyType(pem);
        }
        var total = 0;
        for (var i = 0; i < this._data.length; i++) total += this._data[i].length;
        var merged = new Uint8Array(total);
        var off = 0;
        for (var i = 0; i < this._data.length; i++) { merged.set(this._data[i], off); off += this._data[i].length; }
        var sigBuf = typeof signature === "string"
          ? BufferCtor.from(signature, signatureEncoding || "base64")
          : toBytes(signature);
        var padding = 1;
        var saltLength = 32;
        if (key instanceof KeyObject) {
          // KeyObject does not carry padding; default PKCS1
        } else if (typeof key === "object" && key !== null && !(key instanceof Uint8Array)) {
          if (key.padding !== undefined) padding = key.padding;
          if (key.saltLength !== undefined) saltLength = key.saltLength;
        }
        if (padding === 6 && keyType === "rsa") {
          return natives.cryptoVerifyPss(this._algorithm, merged, pem, sigBuf, saltLength);
        }
        return natives.cryptoVerify(this._algorithm, merged, pem, sigBuf, keyType);
      }
      _transform(chunk, encoding, callback) {
        this.update(chunk, encoding);
        callback();
      }
    }'''

if old_verify not in content:
    print("ERROR: Could not find old Verify class")
    exit(1)

content = content.replace(old_verify, new_verify, 1)
print("Replaced Verify class to extend stream.Transform with _transform()")

with open(FILE, "w", encoding="utf-8") as f:
    f.write(content)

print("Done: Sign/Verify now extend stream.Transform with streaming support")
