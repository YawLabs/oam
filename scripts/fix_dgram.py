"""Replace the dgram stub in node_compat.js with a working UDP implementation."""

import pathlib, sys

JS = pathlib.Path(__file__).resolve().parent.parent / "js" / "node_compat.js"
src = JS.read_text(encoding="utf-8")

OLD = """\
  // ------------------------------------------------------------------ dgram
  registry.factories.dgram = () => {
    const EventEmitter = registry.get("events");
    function createSocket(_type, _callback) {
      const socket = new EventEmitter();
      socket.bind = function () {
        process.nextTick(() =>
          socket.emit("error", new Error("dgram is not implemented in oam")),
        );
        return socket;
      };
      socket.send = function () {
        const cb = arguments[arguments.length - 1];
        if (typeof cb === "function")
          cb(new Error("dgram.send is not implemented in oam"));
      };
      socket.close = function (cb) {
        if (typeof cb === "function") cb();
      };
      socket.address = function () {
        return { address: "0.0.0.0", family: "IPv4", port: 0 };
      };
      socket.addMembership = function () {};
      socket.dropMembership = function () {};
      socket.setBroadcast = function () {};
      socket.setMulticastLoopback = function () {};
      socket.setMulticastTTL = function () {};
      socket.setTTL = function () {};
      socket.ref = function () {
        return socket;
      };
      socket.unref = function () {
        return socket;
      };
      return socket;
    }
    return { createSocket };
  };"""

NEW = """\
  // ------------------------------------------------------------------ dgram
  registry.factories.dgram = (natives) => {
    const EventEmitter = registry.get("events");

    // base64 decode helper (browser-compat atob is available in the snapshot)
    function b64ToBuffer(b64) {
      const raw = atob(b64);
      const buf = globalThis.Buffer.alloc(raw.length);
      for (let i = 0; i < raw.length; i++) buf[i] = raw.charCodeAt(i);
      return buf;
    }

    class Socket extends EventEmitter {
      constructor(type, listener) {
        super();
        this._type = type || "udp4";
        this._handle = null;
        this._bound = false;
        this._closed = false;
        this._recvLoop = false;
        this._address = { address: "0.0.0.0", family: "IPv4", port: 0 };
        if (typeof listener === "function") this.on("message", listener);
      }

      bind(...args) {
        let port = 0, address = "0.0.0.0", cb;
        if (typeof args[0] === "number") {
          port = args[0];
          if (typeof args[1] === "string") address = args[1];
          if (typeof args[args.length - 1] === "function") cb = args[args.length - 1];
        } else if (typeof args[0] === "object" && args[0] !== null) {
          const opts = args[0];
          port = opts.port || 0;
          address = opts.address || "0.0.0.0";
          if (typeof args[1] === "function") cb = args[1];
        } else if (typeof args[0] === "function") {
          cb = args[0];
        }

        if (cb) this.once("listening", cb);

        natives.udpBind(address, port).then((result) => {
          if (this._closed) return;
          this._handle = result.handle;
          this._bound = true;
          this._address = {
            address: result.address,
            port: result.port,
            family: result.family,
          };
          this.emit("listening");
          this._startRecv();
        }).catch((err) => {
          this.emit("error", err);
        });

        return this;
      }

      _startRecv() {
        if (this._recvLoop || this._closed) return;
        this._recvLoop = true;
        const loop = async () => {
          while (!this._closed && this._handle !== null) {
            try {
              const result = await natives.udpRecv(this._handle, 65536);
              if (result === undefined || this._closed) break;
              const msg = b64ToBuffer(result.data);
              this.emit("message", msg, result.rinfo);
            } catch (err) {
              if (!this._closed) this.emit("error", err);
              break;
            }
          }
          this._recvLoop = false;
        };
        loop();
      }

      send(msg, ...args) {
        // Signatures:
        //   send(msg, offset, length, port, address, callback)
        //   send(msg, port, address, callback)
        let offset, length, port, address, cb;

        if (typeof args[0] === "number" && typeof args[1] === "number" &&
            typeof args[2] === "number") {
          // send(msg, offset, length, port, address, callback)
          offset = args[0];
          length = args[1];
          port = args[2];
          address = typeof args[3] === "string" ? args[3] : "127.0.0.1";
          cb = typeof args[args.length - 1] === "function" ? args[args.length - 1] : undefined;
        } else {
          // send(msg, port, address, callback)
          port = args[0];
          address = typeof args[1] === "string" ? args[1] : "127.0.0.1";
          cb = typeof args[args.length - 1] === "function" ? args[args.length - 1] : undefined;
          offset = 0;
          length = undefined;
        }

        let data;
        if (typeof msg === "string") {
          data = globalThis.Buffer.from(msg, "utf8");
        } else if (msg instanceof Uint8Array) {
          data = msg;
        } else if (Array.isArray(msg)) {
          data = globalThis.Buffer.concat(msg.map((m) =>
            typeof m === "string" ? globalThis.Buffer.from(m, "utf8") : m
          ));
        } else {
          data = globalThis.Buffer.from(String(msg));
        }

        if (offset !== undefined && offset !== 0 || length !== undefined) {
          data = data.slice(offset || 0, length !== undefined ? (offset || 0) + length : undefined);
        }

        const doSend = () => {
          natives.udpSend(this._handle, data, String(address), port).then((result) => {
            if (cb) cb(null, result.bytesSent);
          }).catch((err) => {
            if (cb) cb(err);
            else this.emit("error", err);
          });
        };

        if (!this._bound) {
          // Auto-bind like Node does when sending without bind
          this.bind(0, () => doSend());
        } else {
          doSend();
        }
      }

      close(cb) {
        if (this._closed) return this;
        this._closed = true;
        if (this._handle !== null) {
          natives.udpClose(this._handle);
          this._handle = null;
        }
        this._bound = false;
        if (typeof cb === "function") this.once("close", cb);
        process.nextTick(() => this.emit("close"));
        return this;
      }

      address() {
        return Object.assign({}, this._address);
      }

      // Stubs for multicast/TTL options -- no-ops but don't throw
      addMembership() {}
      dropMembership() {}
      setBroadcast() {}
      setMulticastLoopback() {}
      setMulticastTTL() {}
      setTTL() {}
      setRecvBufferSize() {}
      setSendBufferSize() {}
      getRecvBufferSize() { return 65536; }
      getSendBufferSize() { return 65536; }

      ref() { return this; }
      unref() { return this; }
    }

    function createSocket(type, listener) {
      if (typeof type === "object") {
        listener = type.listener || listener;
        type = type.type;
      }
      return new Socket(type, listener);
    }

    return { createSocket, Socket };
  };"""

if OLD not in src:
    print("ERROR: could not find the dgram stub to replace", file=sys.stderr)
    sys.exit(1)

src = src.replace(OLD, NEW, 1)
JS.write_text(src, encoding="utf-8")
print("OK: dgram factory replaced")
