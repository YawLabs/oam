#!/usr/bin/env python3
"""Fix fork child disconnect: destroy socket instead of end to release event loop."""
import pathlib

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")

# Fix child-side disconnect: destroy instead of end
OLD = '''      globalThis.process.disconnect = function disconnect() {
        if (_ipcSock) _ipcSock.end();
        globalThis.process.connected = false;
        globalThis.process.emit("disconnect");
      };'''

NEW = '''      globalThis.process.disconnect = function disconnect() {
        if (_ipcSock) { _ipcSock.destroy(); _ipcSock = null; }
        globalThis.process.connected = false;
        globalThis.process.emit("disconnect");
      };'''

assert OLD in src, "child disconnect not found"
src = src.replace(OLD, NEW, 1)

# Also fix parent-side disconnect
OLD2 = '''      cp.disconnect = function disconnect() {
        if (ipcSocket) {
          ipcSocket.end();
          ipcSocket = null;
        }
        cp.connected = false;
        cp.emit("disconnect");
      };'''

NEW2 = '''      cp.disconnect = function disconnect() {
        if (ipcSocket) {
          ipcSocket.destroy();
          ipcSocket = null;
        }
        cp.connected = false;
        cp.emit("disconnect");
      };'''

assert OLD2 in src, "parent disconnect not found"
src = src.replace(OLD2, NEW2, 1)

p.write_text(src, encoding="utf-8")
print("OK -- disconnect uses destroy()")
