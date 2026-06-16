#!/usr/bin/env python3
"""Fix fork spawnWait handler: capture ipcSocket ref before nulling, close ipcServer."""
import pathlib

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")

OLD = '''          natives.spawnWait(info.handle).then((result) => {
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

NEW = '''          natives.spawnWait(info.handle).then((result) => {
            cp._exited = true;
            cp.exitCode = result.code;
            cp.signalCode = result.signal;
            ipcServer.close();
            if (ipcSocket) {
              var sock = ipcSocket;
              ipcSocket = null;
              sock._chain.then(function() { sock.end(); });
            }
            cp.connected = false;
            cp.emit("exit", result.code, result.signal);
            queueMicrotask(() => cp.emit("close", result.code, result.signal));
          });'''

assert OLD in src, "spawnWait handler not found"
src = src.replace(OLD, NEW, 1)
p.write_text(src, encoding="utf-8")
print("OK -- spawnWait captures socket ref before nulling")
