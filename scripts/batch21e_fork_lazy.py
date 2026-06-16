#!/usr/bin/env python3
"""Fix fork child: lazy IPC connect on first process.on('message') listener."""
import pathlib

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")

# Replace the entire child-side IPC block with a lazy-connect approach
OLD_IPC = r'''    // Fork IPC child side: connect back to parent if OAM_FORK_IPC_PORT is set.
    // Deferred via queueMicrotask because the core runtime (tokio, TCP ops)
    // is not available during installRuntimeGlobals -- it is stored immediately after.
    const _ipcPort = globalThis.process.env.OAM_FORK_IPC_PORT;
    if (_ipcPort) {
      globalThis.process.connected = true;
      const _ipcPending = [];
      let _ipcReady = false;
      let _ipcSock = null;

      globalThis.process.send = function send(message, _sendHandle, _options, callback) {
        if (typeof _sendHandle === "function") { callback = _sendHandle; }
        else if (typeof _options === "function") { callback = _options; }
        if (!globalThis.process.connected) {
          const err = new Error("channel closed");
          err.code = "ERR_IPC_CHANNEL_CLOSED";
          if (callback) callback(err);
          return false;
        }
        const line = JSON.stringify(message) + "\n";
        if (_ipcReady && _ipcSock) {
          _ipcSock.write(line, "utf8", callback);
        } else {
          _ipcPending.push({ line, callback });
        }
        return true;
      };

      globalThis.process.disconnect = function disconnect() {
        if (_ipcSock) { _ipcSock.destroy(); _ipcSock = null; }
        globalThis.process.connected = false;
        globalThis.process.emit("disconnect");
      };

      queueMicrotask(() => {
        const _net = registry.get("net");
        _ipcSock = new _net.Socket();
        let _ipcBuf = "";

        _ipcSock.connect(parseInt(_ipcPort, 10), "127.0.0.1", () => {
          _ipcReady = true;
          _ipcSock.setEncoding("utf8");

          for (const p of _ipcPending) {
            _ipcSock.write(p.line, "utf8", p.callback);
          }
          _ipcPending.length = 0;

          _ipcSock.on("data", (chunk) => {
            _ipcBuf += chunk;
            let nl;
            while ((nl = _ipcBuf.indexOf("\n")) !== -1) {
              const line = _ipcBuf.slice(0, nl);
              _ipcBuf = _ipcBuf.slice(nl + 1);
              try {
                const msg = JSON.parse(line);
                globalThis.process.emit("message", msg);
              } catch (_) { /* ignore malformed */ }
            }
          });
          _ipcSock.on("end", () => {
            globalThis.process.connected = false;
            globalThis.process.emit("disconnect");
          });
          _ipcSock.on("error", () => {
            globalThis.process.connected = false;
          });
        });
      });
    }'''

# New approach: store port in process._ipcPort, lazy-connect when process.on('message') is first called.
# The connect happens during event loop (after CoreRuntime exists), not during installRuntimeGlobals.
NEW_IPC = r'''    // Fork IPC child side: store port for lazy connect.
    // Cannot connect during installRuntimeGlobals because CoreRuntime
    // (tokio, TCP ops) is not installed until execute_module/reset_run_slots.
    // Instead, store the port and connect lazily on first process.on('message').
    const _ipcPort = globalThis.process.env.OAM_FORK_IPC_PORT;
    if (_ipcPort) {
      globalThis.process.connected = true;
      let _ipcSock = null;
      let _ipcReady = false;
      const _ipcPending = [];
      let _ipcConnecting = false;

      function _ipcEnsureConnect() {
        if (_ipcConnecting || _ipcReady) return;
        _ipcConnecting = true;
        const _net = registry.get("net");
        _ipcSock = new _net.Socket();
        let _ipcBuf = "";

        _ipcSock.connect(parseInt(_ipcPort, 10), "127.0.0.1", () => {
          _ipcReady = true;
          _ipcSock.setEncoding("utf8");

          for (const p of _ipcPending) {
            _ipcSock.write(p.line, "utf8", p.callback);
          }
          _ipcPending.length = 0;

          _ipcSock.on("data", (chunk) => {
            _ipcBuf += chunk;
            let nl;
            while ((nl = _ipcBuf.indexOf("\n")) !== -1) {
              const line = _ipcBuf.slice(0, nl);
              _ipcBuf = _ipcBuf.slice(nl + 1);
              try {
                const msg = JSON.parse(line);
                globalThis.process.emit("message", msg);
              } catch (_) { /* ignore malformed */ }
            }
          });
          _ipcSock.on("end", () => {
            globalThis.process.connected = false;
            globalThis.process.emit("disconnect");
          });
          _ipcSock.on("error", () => {
            globalThis.process.connected = false;
          });
        });
      }

      const _origOn = globalThis.process.on.bind(globalThis.process);
      globalThis.process.on = function on(event, listener) {
        if (event === "message") _ipcEnsureConnect();
        return _origOn(event, listener);
      };

      globalThis.process.send = function send(message, _sendHandle, _options, callback) {
        if (typeof _sendHandle === "function") { callback = _sendHandle; }
        else if (typeof _options === "function") { callback = _options; }
        if (!globalThis.process.connected) {
          const err = new Error("channel closed");
          err.code = "ERR_IPC_CHANNEL_CLOSED";
          if (callback) callback(err);
          return false;
        }
        _ipcEnsureConnect();
        const line = JSON.stringify(message) + "\n";
        if (_ipcReady && _ipcSock) {
          _ipcSock.write(line, "utf8", callback);
        } else {
          _ipcPending.push({ line, callback });
        }
        return true;
      };

      globalThis.process.disconnect = function disconnect() {
        if (_ipcSock) { _ipcSock.destroy(); _ipcSock = null; }
        globalThis.process.connected = false;
        globalThis.process.emit("disconnect");
      };
    }'''

assert OLD_IPC in src, "old IPC block not found"
src = src.replace(OLD_IPC, NEW_IPC, 1)
p.write_text(src, encoding="utf-8")
print("OK -- lazy IPC connect")
