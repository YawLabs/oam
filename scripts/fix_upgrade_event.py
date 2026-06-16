#!/usr/bin/env python3
"""Patch node_compat.js to emit 'upgrade' events from the HTTP server accept loop.

Uses exact byte matching to avoid smart-quote corruption on Windows ARM64.
"""

import sys, os

path = os.path.join(os.path.dirname(__file__), '..', 'js', 'node_compat.js')
with open(path, 'r', encoding='utf-8') as f:
    src = f.read()

# --- Patch 1: modify the accept loop to detect upgrade requests ---
old_accept_loop = """            (async () => {
              for (;;) {
                const meta = await natives.httpAccept(bound.serverId);
                if (meta === undefined) break;
                const req = new IncomingMessage(meta);
                const res = new ServerResponse(meta.requestId);
                this.emit("request", req, res);
              }
              this.emit("close");
            })();"""

new_accept_loop = """            (async () => {
              for (;;) {
                const meta = await natives.httpAccept(bound.serverId);
                if (meta === undefined) break;
                if (meta.isUpgrade && meta.socketHandle !== undefined) {
                  const socket = new netMod.Socket({
                    _handle: meta.socketHandle,
                    _remoteAddr: {
                      address: meta.remoteAddress || "127.0.0.1",
                      port: meta.remotePort || 0,
                      family: "IPv4",
                    },
                  });
                  socket._readLoop();
                  const req = new IncomingMessage(meta);
                  this.emit("upgrade", req, socket, globalThis.Buffer.alloc(0));
                } else {
                  const req = new IncomingMessage(meta);
                  const res = new ServerResponse(meta.requestId);
                  this.emit("request", req, res);
                }
              }
              this.emit("close");
            })();"""

if old_accept_loop not in src:
    print("ERROR: could not find accept loop to patch", file=sys.stderr)
    sys.exit(1)

src = src.replace(old_accept_loop, new_accept_loop, 1)

with open(path, 'w', encoding='utf-8', newline='\n') as f:
    f.write(src)

print("Patched accept loop for upgrade events")
