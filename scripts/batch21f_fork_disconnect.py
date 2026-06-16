#!/usr/bin/env python3
"""Fix fork IPC: disconnect must drain pending writes before destroying the socket.

Both child-side process.disconnect() and parent-side cp.disconnect() call
socket.destroy() synchronously, which drops any writes still in the Socket._chain
promise queue. process.send() followed by process.disconnect() is idiomatic Node
and must work -- the send must complete before the socket is torn down.

Fix: chain the destroy onto the socket's write chain so pending writes flush first.
"""
import pathlib

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")

# ---------- child-side disconnect ----------
OLD_CHILD = '''      globalThis.process.disconnect = function disconnect() {
        if (_ipcSock) { _ipcSock.destroy(); _ipcSock = null; }
        globalThis.process.connected = false;
        globalThis.process.emit("disconnect");
      };'''

NEW_CHILD = '''      globalThis.process.disconnect = function disconnect() {
        globalThis.process.connected = false;
        if (_ipcSock) {
          var sock = _ipcSock;
          _ipcSock = null;
          _ipcReady = false;
          sock._chain.then(function() {
            sock.destroy();
            globalThis.process.emit("disconnect");
          });
        } else {
          globalThis.process.emit("disconnect");
        }
      };'''

assert OLD_CHILD in src, "child disconnect block not found"
src = src.replace(OLD_CHILD, NEW_CHILD, 1)

# ---------- parent-side disconnect ----------
OLD_PARENT = '''      cp.disconnect = function disconnect() {
        if (ipcSocket) {
          ipcSocket.destroy();
          ipcSocket = null;
        }
        cp.connected = false;
        cp.emit("disconnect");
      };'''

NEW_PARENT = '''      cp.disconnect = function disconnect() {
        cp.connected = false;
        if (ipcSocket) {
          var sock = ipcSocket;
          ipcSocket = null;
          sock._chain.then(function() {
            sock.destroy();
            cp.emit("disconnect");
          });
        } else {
          cp.emit("disconnect");
        }
      };'''

assert OLD_PARENT in src, "parent disconnect block not found"
src = src.replace(OLD_PARENT, NEW_PARENT, 1)

p.write_text(src, encoding="utf-8")
print("OK -- disconnect drains writes before destroy")
