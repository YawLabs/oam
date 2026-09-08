// Stand-in for the oam binary, driven through the launcher by pointing
// OAMJS_BINARY at the running Node and passing this file as the first argument.
// Using a real child process (rather than stubbing spawn) is the point: the
// stdio and exit-status claims the launcher makes are claims about the
// operating system, and a stub would only prove the launcher agrees with
// itself.
const [mode, ...rest] = process.argv.slice(2);

switch (mode) {
  case 'exit':
    process.exit(Number(rest[0]));
    break;

  case 'argv':
    // JSON so an argument that was silently split, re-quoted or backslash-eaten
    // by the Windows command-line round trip shows up as a different array
    // rather than as similar-looking text.
    process.stdout.write(JSON.stringify(rest));
    break;

  case 'echo': {
    // Byte-for-byte stdin -> stdout, plus a marker on stderr so the test can
    // prove the two streams did not get merged. This is the MCP case: a stdio
    // server frames its protocol on the stream, so a wrapper that re-encodes,
    // buffers into lines, or interleaves stderr breaks it.
    const chunks = [];
    process.stdin.on('data', (c) => chunks.push(c));
    process.stdin.on('end', () => {
      process.stderr.write('child-stderr-marker');
      process.stdout.write(Buffer.concat(chunks));
    });
    break;
  }

  case 'signal':
    // Report the signal that killed us by dying from it, after telling the test
    // we are ready to be killed. Never exits on its own: the timer is what
    // keeps the loop alive if the signal never arrives, so a broken forward
    // shows up as a test timeout rather than a pass.
    process.stdout.write('ready\n');
    setTimeout(() => process.exit(0), 30_000);
    break;

  default:
    process.stderr.write(`child fixture: unknown mode ${mode}\n`);
    process.exit(64);
}
