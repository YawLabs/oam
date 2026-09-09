# oamjs

**The reliable TypeScript runtime for the AI era.** oam is a JavaScript and
TypeScript runtime built in Rust on V8: it runs `.ts` directly, and it is built
to host MCP servers over stdio.

This package is a launcher. It contains no binary itself -- it resolves the
per-platform package npm installed for your machine and exec's the `oam` binary
inside it.

```sh
npx oamjs script.ts
npx oamjs -pe "2+2"
```

Or pin it in the project you already have a lockfile for:

```sh
npm install --save-dev oamjs
./node_modules/.bin/oamjs --version
```

Full documentation, and the standalone installer that does not involve npm at
all: <https://oamjs.org> and <https://github.com/YawLabs/oam>.

## Status

Beta. Breaking changes before 1.0 are possible and are called out in the
repository's CHANGELOG.md. There is no LTS yet.

## Supported platforms

| Platform | Binary |
|---|---|
| Windows x64 | `oamjs-win32-x64` (`x86_64-pc-windows-msvc`) |
| Windows arm64 | `oamjs-win32-arm64` (`aarch64-pc-windows-msvc`) |
| macOS x64 | `oamjs-darwin-x64` (`x86_64-apple-darwin`) |
| macOS arm64 | `oamjs-darwin-arm64` (`aarch64-apple-darwin`) |
| Linux x64, glibc | `oamjs-linux-x64` (`x86_64-unknown-linux-gnu`) |

Those five are installed as `optionalDependencies`; npm downloads only the one
matching your `os` and `cpu`.

**Linux arm64 is not published.** oam's V8 startup snapshot has to be generated
by a process of the target architecture, so it cannot be cross-compiled, and no
ARM Linux host has been in the release loop. Running `oamjs` there prints that
plainly rather than failing as a broken install. Build from source, or use an
x86_64 host.

**musl (Alpine) is not published either.** The only Linux binary is the glibc
one. The launcher detects a musl host and says so, because the alternative is
the dynamic loader reporting "No such file or directory" about a file that is
plainly there.

## No install scripts

This package has no `postinstall`, `preinstall` or `prepare` script, and neither
does any of the per-platform packages. The binary ships inside the tarball npm
already downloads. That is a deliberate choice, not an oversight: oam ships
`oam trust` and diagnostic OAM-PKG0007 precisely because arbitrary code at
install time is a supply-chain problem, and it would be incoherent to argue that
and then run a download script in your `npm ci`. It also means this package
installs behind a proxy, in an offline CI cache, and in a sandbox with no
network at install time.

## Environment

| Variable | Effect |
|---|---|
| `OAMJS_BINARY` | Exec this path instead of resolving a per-platform package. For pointing the launcher at a locally built `target/release/oam`. |

Everything else on the command line is oam's; the launcher forwards argv, stdin,
stdout, stderr and exit status unchanged, and forwards signals to the runtime so
that a supervisor stopping `oamjs` stops oam.

## License

Apache-2.0. Each per-platform package ships `LICENSE`, `NOTICE` and
`THIRD_PARTY_LICENSES.md` beside its binary: an oam binary statically links V8,
ICU, a port of Node's streams and around 380 Rust crates, and those notices
travel with every copy.
