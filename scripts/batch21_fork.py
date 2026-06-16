#!/usr/bin/env python3
"""Patch node_compat.js: implement child_process.fork with TCP-based IPC."""
import pathlib

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")

# 1) Replace the fork stub in child_process
OLD_FORK = '''      fork: () => { throw new Error("child_process.fork is not implemented in oam yet"); },'''
NEW_FORK = '''      fork: fork,'''

assert OLD_FORK in src, "fork stub not found"
src = src.replace(OLD_FORK, NEW_FORK, 1)

# 2) Insert the fork function before the return statement in child_process factory
# Find the right spot -- after execFile function, before the return
OLD_RETURN = '''    return {
      spawn,
      spawnSync,
      exec,
      execSync,
      execFile,
      execFileSync,
      fork: fork,
      ChildProcess,
    };'''

NEW_RETURN = r'''    function fork(modulePath, args, options) {
      if (typeof args === "object" && !Array.isArray(args)) {
        options = args;
        args = [];
      }
      args = (args || []).map(String);
      const opts = Object.assign({}, options);
      const execPath = opts.execPath || globalThis.process.execPath;
      const execArgv = opts.execArgv || globalThis.process.execArgv || [];

      const cp = new ChildProcess();
      cp.connected = true;
      cp._ipcBuffer = "";

      const net = registry.get("net");
      const ipcServer = net.createServer();

      ipcServer.listen(0, "127.0.0.1", () => {
        const ipcPort = ipcServer.address().port;

        const childEnv = Object.assign({},
          opts.env || globalThis.process.env,
          { OAM_FORK_IPC_PORT: String(ipcPort) },
        );

        const spawnArgs = execArgv.concat(["run", String(modulePath), "--no-check", "--"]).concat(args);
        const nativeOpts = {
          cwd: opts.cwd || undefined,
          env: childEnv,
          shell: false,
          clearEnv: false,
        };

        let ipcSocket = null;

        ipcServer.on("connection", (socket) => {
          ipcSocket = socket;
          ipcServer.close();

          let buf = "";
          socket.setEncoding("utf8");
          socket.on("data", (chunk) => {
            buf += chunk;
            let nl;
            while ((nl = buf.indexOf("\n")) !== -1) {
              const line = buf.slice(0, nl);
              buf = buf.slice(nl + 1);
              try {
                const msg = JSON.parse(line);
                cp.emit("message", msg);
              } catch (_) { /* ignore malformed */ }
            }
          });
          socket.on("end", () => {
            cp.connected = false;
            cp.emit("disconnect");
          });
          socket.on("error", () => {
            cp.connected = false;
          });
        });

        cp.send = function send(message, _sendHandle, _options, callback) {
          if (typeof _sendHandle === "function") { callback = _sendHandle; }
          else if (typeof _options === "function") { callback = _options; }
          if (!cp.connected || !ipcSocket) {
            const err = new Error("channel closed");
            err.code = "ERR_IPC_CHANNEL_CLOSED";
            if (callback) callback(err);
            return false;
          }
          ipcSocket.write(JSON.stringify(message) + "\n", "utf8", callback);
          return true;
        };

        cp.disconnect = function disconnect() {
          if (ipcSocket) {
            ipcSocket.end();
            ipcSocket = null;
          }
          cp.connected = false;
          cp.emit("disconnect");
        };

        natives.spawnAsync(execPath, spawnArgs, nativeOpts).then((info) => {
          cp._handle = info.handle;
          cp.pid = info.pid;

          cp.stdout = new Readable({ read() {} });
          cp.stderr = new Readable({ read() {} });
          cp.stdin = new Writable({
            write(chunk, encoding, callback) {
              natives.spawnWrite(info.handle, typeof chunk === "string" ? Buffer.from(chunk, encoding) : chunk)
                .then(() => callback(), (err) => callback(err));
            },
          });

          cp.emit("spawn");

          const readStdout = async (handle) => {
            while (true) {
              const chunk = await natives.spawnReadStdout(handle);
              if (chunk === undefined || chunk === null || chunk.length === 0) {
                cp.stdout.push(null);
                break;
              }
              cp.stdout.push(Buffer.from(chunk));
            }
          };
          const readStderr = async (handle) => {
            while (true) {
              const chunk = await natives.spawnReadStderr(handle);
              if (chunk === undefined || chunk === null || chunk.length === 0) {
                cp.stderr.push(null);
                break;
              }
              cp.stderr.push(Buffer.from(chunk));
            }
          };
          readStdout(info.handle);
          readStderr(info.handle);

          natives.spawnWait(info.handle).then((result) => {
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
          });
        }).catch((err) => {
          ipcServer.close();
          queueMicrotask(() => cp.emit("error", typeof err === "string" ? new Error(err) : err));
        });
      });

      return cp;
    }

    return {
      spawn,
      spawnSync,
      exec,
      execSync,
      execFile,
      execFileSync,
      fork: fork,
      ChildProcess,
    };'''

assert OLD_RETURN in src, "old return block not found"
src = src.replace(OLD_RETURN, NEW_RETURN, 1)

# 3) Add child-side IPC setup in installRuntimeGlobals
OLD_RUNTIME = '''    globalThis.process = registry.get("process");'''
NEW_RUNTIME = r'''    globalThis.process = registry.get("process");

    // Fork IPC child side: connect back to parent if OAM_FORK_IPC_PORT is set
    const _ipcPort = globalThis.process.env.OAM_FORK_IPC_PORT;
    if (_ipcPort) {
      const _net = registry.get("net");
      const _ipcSock = new _net.Socket();
      let _ipcBuf = "";
      globalThis.process.connected = true;

      globalThis.process.send = function send(message, _sendHandle, _options, callback) {
        if (typeof _sendHandle === "function") { callback = _sendHandle; }
        else if (typeof _options === "function") { callback = _options; }
        if (!globalThis.process.connected) {
          const err = new Error("channel closed");
          err.code = "ERR_IPC_CHANNEL_CLOSED";
          if (callback) callback(err);
          return false;
        }
        _ipcSock.write(JSON.stringify(message) + "\n", "utf8", callback);
        return true;
      };

      globalThis.process.disconnect = function disconnect() {
        _ipcSock.end();
        globalThis.process.connected = false;
        globalThis.process.emit("disconnect");
      };

      _ipcSock.connect(parseInt(_ipcPort, 10), "127.0.0.1", () => {
        _ipcSock.setEncoding("utf8");
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
    }'''

assert OLD_RUNTIME in src, "runtime globals hook not found"
src = src.replace(OLD_RUNTIME, NEW_RUNTIME, 1)

p.write_text(src, encoding="utf-8")
print("OK -- fork implementation patched")
