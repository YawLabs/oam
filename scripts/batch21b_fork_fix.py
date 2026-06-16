#!/usr/bin/env python3
"""Fix fork: buffer sends before IPC connects, set up stdout/stderr eagerly."""
import pathlib

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")

# Replace the entire fork function
OLD_FORK = r'''    function fork(modulePath, args, options) {
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
    }'''

NEW_FORK = r'''    function fork(modulePath, args, options) {
      if (typeof args === "object" && !Array.isArray(args)) {
        options = args;
        args = [];
      }
      args = (args || []).map(String);
      const opts = Object.assign({}, options);
      const execPath = opts.execPath || globalThis.process.execPath;
      const execArgv = opts.execArgv || globalThis.process.execArgv || [];
      const silent = !!opts.silent;

      const cp = new ChildProcess();
      cp.connected = true;

      cp.stdout = silent ? new Readable({ read() {} }) : null;
      cp.stderr = silent ? new Readable({ read() {} }) : null;
      cp.stdin = null;

      let ipcSocket = null;
      const pendingSends = [];

      cp.send = function send(message, _sendHandle, _options, callback) {
        if (typeof _sendHandle === "function") { callback = _sendHandle; }
        else if (typeof _options === "function") { callback = _options; }
        if (!cp.connected) {
          const err = new Error("channel closed");
          err.code = "ERR_IPC_CHANNEL_CLOSED";
          if (callback) callback(err);
          return false;
        }
        const line = JSON.stringify(message) + "\n";
        if (ipcSocket) {
          ipcSocket.write(line, "utf8", callback);
        } else {
          pendingSends.push({ line, callback });
        }
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

        ipcServer.on("connection", (socket) => {
          ipcSocket = socket;
          ipcServer.close();

          for (const p of pendingSends) {
            socket.write(p.line, "utf8", p.callback);
          }
          pendingSends.length = 0;

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

        natives.spawnAsync(execPath, spawnArgs, nativeOpts).then((info) => {
          cp._handle = info.handle;
          cp.pid = info.pid;

          if (silent) {
            cp.stdin = new Writable({
              write(chunk, encoding, callback) {
                natives.spawnWrite(info.handle, typeof chunk === "string" ? Buffer.from(chunk, encoding) : chunk)
                  .then(() => callback(), (err) => callback(err));
              },
            });
          }

          cp.emit("spawn");

          const readStdout = async (handle) => {
            while (true) {
              const chunk = await natives.spawnReadStdout(handle);
              if (chunk === undefined || chunk === null || chunk.length === 0) {
                if (cp.stdout) cp.stdout.push(null);
                break;
              }
              if (cp.stdout) cp.stdout.push(Buffer.from(chunk));
            }
          };
          const readStderr = async (handle) => {
            while (true) {
              const chunk = await natives.spawnReadStderr(handle);
              if (chunk === undefined || chunk === null || chunk.length === 0) {
                if (cp.stderr) cp.stderr.push(null);
                break;
              }
              if (cp.stderr) cp.stderr.push(Buffer.from(chunk));
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
    }'''

assert OLD_FORK in src, "old fork function not found"
src = src.replace(OLD_FORK, NEW_FORK, 1)

p.write_text(src, encoding="utf-8")
print("OK -- fork fix applied")
