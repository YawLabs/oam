# oamjs-darwin-arm64

The oam binary for darwin arm64 (`aarch64-apple-darwin`).

You do not install this directly. It is an optional dependency of
[oamjs](https://www.npmjs.com/package/oamjs), which npm installs only on a
matching host, and which resolves and exec's the binary in here.

No install scripts. The binary is in the tarball.

Apache-2.0. `LICENSE`, `NOTICE` and `THIRD_PARTY_LICENSES.md` ship beside the
binary: it statically links V8, ICU, a port of Node's streams and around 380
Rust crates, and those notices travel with every copy.

Home: <https://oamjs.org> -- <https://github.com/YawLabs/oam>
