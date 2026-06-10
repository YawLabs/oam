// Ambient declarations for oam's runtime globals (M1 surface). Ships as a
// types package (@yawlabs/oam) with the npm work in M2; until then projects
// carry this file (tsconfig includes it by default). The runtime and these
// declarations must never disagree — that parity gets a conformance test
// when the surface grows.
declare const oam: {
  /** Runtime version string. */
  readonly version: string;
  /** Resolve after ms milliseconds (tokio-backed, keeps the process alive). */
  sleep(ms: number): Promise<void>;
  /** Read a file as UTF-8 text. Rejects with Error on I/O failure. */
  readTextFile(path: string): Promise<string>;
};
