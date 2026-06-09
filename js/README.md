# js/ — runtime-internal TypeScript

Core JS/TS sources compiled into the startup snapshot (M1): primordials, the ODIF-aware
console, ECMA-429 globals' JS halves, and lazily-evaluated `node:` shim registrations.

Empty at M0 — the minimal console is a Rust binding in `oam_engine` until the snapshot
pipeline exists to host the real one.
