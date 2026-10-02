// Regenerates the `oam self-update` verifier fixtures in this directory.
//
//   node crates/oam_cli/tests/fixtures/self_update/generate.mjs [ssh-keygen]
//
// Every signature here is made by a REAL ssh-keygen (-Y sign) with throwaway
// keys that exist only for the length of this script: the private halves are
// never written outside a temp dir, and none of them is a release key. The
// mutated vectors are byte edits of a real signature, and the one vector
// ssh-keygen cannot make without a hardware token (sk-ssh-ed25519) is built
// here by hand -- and then checked by ssh-keygen -Y verify like every other
// vector. The script fails unless ssh-keygen's verdict on each vector is the
// one recorded below, so a committed fixture always means what its name says.
//
// Regenerating replaces every key, so all files move together; commit them
// as one change. The Rust tests (src/self_update.rs) read the result.

import { execFileSync } from "node:child_process";
import { createHash, generateKeyPairSync, sign } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const KEYGEN = process.argv[2] || "ssh-keygen";
const NS = "oam-release";
const work = mkdtempSync(join(tmpdir(), "oam-su-fixtures-"));
const cases = join(HERE, "cases");
mkdirSync(cases, { recursive: true });

const keygen = (args, opts = {}) =>
  execFileSync(KEYGEN, args, { stdio: ["pipe", "pipe", "pipe"], ...opts });

// --- SSH wire encoding ---------------------------------------------------------
const u32 = (n) => {
  const b = Buffer.alloc(4);
  b.writeUInt32BE(n >>> 0);
  return b;
};
const str = (x) => {
  const b = Buffer.isBuffer(x) ? x : Buffer.from(x);
  return Buffer.concat([u32(b.length), b]);
};
// Read SSH strings off a buffer in order.
function reader(buf) {
  let off = 0;
  return {
    raw(n) {
      const v = buf.subarray(off, off + n);
      off += n;
      return v;
    },
    u32() {
      const v = buf.readUInt32BE(off);
      off += 4;
      return v;
    },
    str() {
      const n = this.u32();
      return this.raw(n);
    },
    get off() {
      return off;
    },
  };
}
const armor = (blob) => {
  const b64 = blob.toString("base64").match(/.{1,70}/g).join("\n");
  return `-----BEGIN SSH SIGNATURE-----\n${b64}\n-----END SSH SIGNATURE-----\n`;
};
const dearmor = (text) =>
  Buffer.from(
    text
      .replace(/-----(BEGIN|END) SSH SIGNATURE-----/g, "")
      .replace(/\s+/g, ""),
    "base64",
  );
// Split a real SSHSIG blob into its fields (offsets kept for byte edits).
function parseSig(blob) {
  const r = reader(blob);
  const magic = r.raw(6);
  const version = r.u32();
  const pubkey = r.str();
  const namespace = r.str();
  const reserved = r.str();
  const hashAlg = r.str();
  const signature = r.str();
  return { magic, version, pubkey, namespace, reserved, hashAlg, signature, end: r.off };
}
const buildSig = (f) =>
  Buffer.concat([
    f.magic,
    u32(f.version),
    str(f.pubkey),
    str(f.namespace),
    str(f.reserved),
    str(f.hashAlg),
    str(f.signature),
  ]);
const pubBlob = (pubLine) => Buffer.from(pubLine.trim().split(/\s+/)[1], "base64");

// --- keys --------------------------------------------------------------------
const keys = {};
for (const id of ["t1", "t2", "t3"]) {
  keygen(["-q", "-t", "ed25519", "-N", "", "-C", `oam-release-${id}`, "-f", join(work, id)]);
  keys[id] = readFileSync(join(work, `${id}.pub`), "utf8").trim().split(/\s+/).slice(0, 2).join(" ");
}
// The sk key: a plain ed25519 key wrapped as sk-ssh-ed25519@openssh.com with
// ssh-keygen's default application, "ssh:".
const sk = generateKeyPairSync("ed25519");
const skRaw = Buffer.from(sk.publicKey.export({ format: "jwk" }).x, "base64url");
const SK_ALG = "sk-ssh-ed25519@openssh.com";
const SK_APP = "ssh:";
const skBlob = Buffer.concat([str(SK_ALG), str(skRaw), str(SK_APP)]);
keys.tsk = `${SK_ALG} ${skBlob.toString("base64")}`;

// The test trust root: t1 and tsk may sign from v0.18.0; t2 is staged (no
// range); t3 is not listed at all.
const allowed = ["t1", "t2", "tsk"]
  .map((id) => `oam-release-${id} namespaces="${NS}" ${keys[id]}`)
  .join("\n");
writeFileSync(
  join(HERE, "allowed_signers"),
  `# TEST trust root for src/self_update.rs -- throwaway keys, made by generate.mjs.\n${allowed}\n`,
);
writeFileSync(
  join(HERE, "ranges"),
  "# TEST ranges for src/self_update.rs. t2 is staged: no line.\nt1 v0.18.0 -\ntsk v0.18.0 -\n",
);
writeFileSync(join(HERE, "untrusted.pub"), `${keys.t3}\n`);

// --- manifests -----------------------------------------------------------------
const payload = Buffer.from("oam self-update fixture binary: not a real oam\n");
writeFileSync(join(HERE, "payload.bin"), payload);
const h = createHash("sha256").update(payload).digest("hex");
const TRIPLES = [
  "aarch64-apple-darwin",
  "aarch64-pc-windows-msvc.exe",
  "aarch64-unknown-linux-gnu",
  "x86_64-apple-darwin",
  "x86_64-pc-windows-msvc.exe",
  "x86_64-unknown-linux-gnu",
];
const sums = TRIPLES.map((t) => `${h} *oam-${t}\n`).join("");
writeFileSync(join(HERE, "SHA256SUMS"), sums);
const manifest = (tag, header = "oam-release-manifest v1", eol = "\n") =>
  Buffer.from(`${header}${eol}tag ${tag}${eol}${sums}`);
const M18 = manifest("v0.18.0");
writeFileSync(join(HERE, "manifest-v0.18.0"), M18);

function signWith(id, msg, extra = []) {
  const f = join(work, `msg-${Math.random().toString(16).slice(2)}`);
  writeFileSync(f, msg);
  keygen(["-Y", "sign", "-f", join(work, id), "-n", NS, ...extra, f]);
  return readFileSync(`${f}.sig`, "utf8");
}
function signSk(msg, { flags = 0x01, counter = 7, hashAlg = "sha512" } = {}) {
  const H = createHash(hashAlg).update(msg).digest();
  const signed = Buffer.concat([Buffer.from("SSHSIG"), str(NS), str(""), str(hashAlg), str(H)]);
  const fc = Buffer.concat([Buffer.from([flags]), u32(counter)]);
  const inner = Buffer.concat([
    createHash("sha256").update(SK_APP).digest(),
    fc,
    createHash("sha256").update(signed).digest(),
  ]);
  const ed = sign(null, inner, sk.privateKey);
  const sigField = Buffer.concat([str(SK_ALG), str(ed), fc]);
  return armor(
    buildSig({
      magic: Buffer.from("SSHSIG"),
      version: 1,
      pubkey: skBlob,
      namespace: Buffer.from(NS),
      reserved: Buffer.alloc(0),
      hashAlg: Buffer.from(hashAlg),
      signature: sigField,
    }),
  );
}

// --- vectors -----------------------------------------------------------------
// [name, armored sig (null = a fresh t1/principal signature over the manifest),
//  manifest bytes (null = M18), principal to verify as, ssh-keygen's verdict].
// The verdict is ssh-keygen's, not oam's: where they differ, the Rust test
// says so by name.
const valid = signWith("t1", M18);
const validFields = parseSig(dearmor(valid));
const mutate = (edit) => {
  const f = { ...parseSig(dearmor(valid)) };
  edit(f);
  return armor(buildSig(f));
};
const flipped = Buffer.from(validFields.signature);
flipped[flipped.length - 1] ^= 0x01;
const t3sig = parseSig(dearmor(signWith("t3", M18)));
const vectors = [
  ["valid", valid, null, "t1", true],
  ["valid-sha256", signWith("t1", M18, ["-O", "hashalg=sha256"]), null, "t1", true],
  ["sk-valid", signSk(M18), null, "tsk", true],
  ["wrong-namespace", keygenSignNs("t1", M18, "file"), null, "t1", false],
  ["untrusted-key", signWith("t3", M18), null, "t1", false],
  ["staged-key", signWith("t2", M18), null, "t2", true],
  ["truncated", armor(dearmor(valid).subarray(0, dearmor(valid).length - 9)), null, "t1", false],
  ["trailing-bytes", armor(Buffer.concat([dearmor(valid), Buffer.from([0, 0, 0, 0])])), null, "t1", false],
  ["bad-preamble", mutate((f) => (f.magic = Buffer.from("SSHSIH"))), null, "t1", false],
  // ssh-keygen accepts a version 0: it only refuses versions ABOVE 1, and the
  // field is not covered by the signature. oam refuses anything but 1.
  ["version-0", mutate((f) => (f.version = 0)), null, "t1", true],
  ["version-2", mutate((f) => (f.version = 2)), null, "t1", false],
  ["hash-sha384", mutate((f) => (f.hashAlg = Buffer.from("sha384"))), null, "t1", false],
  ["sig-bitflip", mutate((f) => (f.signature = flipped)), null, "t1", false],
  // A t1 signature claiming to be by t2 (also trusted): the pinned key is
  // found, but the signature is not its.
  ["pubkey-swapped", mutate((f) => (f.pubkey = pubBlob(keys.t2))), null, "t2", false],
  // An untrusted key's signature claiming to be by t1.
  ["pubkey-claims-trusted", armor(buildSig({ ...t3sig, pubkey: pubBlob(keys.t1) })), null, "t1", false],
  // Validly signed, wrong content: the Rust side rejects these on content.
  ["tag-mismatch", null, manifest("v0.18.1"), "t1", true],
  ["out-of-range", null, manifest("v0.17.9"), "t1", true],
  ["crlf-header", null, manifest("v0.18.0", "oam-release-manifest v1", "\r\n"), "t1", true],
  ["bad-header", null, manifest("v0.18.0", "oam-release-manifest v2"), "t1", true],
];
function keygenSignNs(id, msg, ns) {
  const f = join(work, `ns-${ns}`);
  writeFileSync(f, msg);
  keygen(["-Y", "sign", "-f", join(work, id), "-n", ns, f]);
  return readFileSync(`${f}.sig`, "utf8");
}
// The sk vector without the user-presence flag. ssh-keygen -Y verify accepts
// it (measured on OpenSSH 10.5p1), and so does oam: the installers verify with
// ssh-keygen, and the two verifiers must agree. A token only signs without a
// touch when its key was made with no-touch-required.
vectors.push(["sk-no-presence", signSk(M18, { flags: 0x00 }), null, "tsk", true]);

let bad = 0;
for (const v of vectors) {
  const [name, , msgOverride, principal, expect] = v;
  const msg = msgOverride ?? M18;
  const sigText = v[1] ?? signWith(principal, msg);
  if (msgOverride) writeFileSync(join(cases, `${name}.manifest`), msg);
  writeFileSync(join(cases, `${name}.sig`), sigText);
  const sigPath = join(work, `${name}.sig`);
  writeFileSync(sigPath, sigText);
  let ok = true;
  try {
    keygen(
      ["-Y", "verify", "-f", join(HERE, "allowed_signers"), "-I", `oam-release-${principal}`, "-n", NS, "-s", sigPath],
      { input: msg },
    );
  } catch {
    ok = false;
  }
  const mark = ok === expect ? "ok  " : "BAD ";
  if (ok !== expect) bad++;
  console.log(`${mark} ${name}: ssh-keygen ${ok ? "accepts" : "rejects"} (expected ${expect ? "accept" : "reject"})`);
}
rmSync(work, { recursive: true, force: true });
if (bad) {
  console.error(`${bad} vector(s) did not get the expected ssh-keygen verdict`);
  process.exit(1);
}
