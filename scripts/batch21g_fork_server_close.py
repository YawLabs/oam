#!/usr/bin/env python3
"""Fix fork: close IPC server when child exits without connecting.

The IPC server stays listening if the child never connects (e.g. a child
that only uses stdout and never calls process.send or process.on('message')).
The orphaned server keeps the parent's event loop alive forever.

Also fix the spawnWait handler to drain the ipcSocket write chain before
ending, matching the disconnect fix from batch21f.
"""
import pathlib

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")

OLD = '''          natives.spawnWait(info.handle).then((result) => {
            cp._exited = true;
            cp.exitCode = result.code;
            cp.signalCode = result.signal;
            if (ipcSocket) {
              ipcSocket.end();
              ipcSocket = null;
            }
            cp.connected = false;
            cp.emit("exit", result.code, result.signal);
            queueMicrotask(() => cp.emit("close", result.code, result.signal));
          });'''

NEW = '''          natives.spawnWait(info.handle).then((result) => {
            cp._exited = true;
            cp.exitCode = result.code;
            cp.signalCode = result.signal;
            ipcServer.close();
            if (ipcSocket) {
              ipcSocket._chain.then(function() { ipcSocket.end(); });
              ipcSocket = null;
            }
            cp.connected = false;
            cp.emit("exit", result.code, result.signal);
            queueMicrotask(() => cp.emit("close", result.code, result.signal));
          });'''

assert OLD in src, "spawnWait handler not found"
src = src.replace(OLD, NEW, 1)
p.write_text(src, encoding="utf-8")
print("OK -- ipcServer.close() on child exit")
