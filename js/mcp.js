// oam:mcp -- MCP server primitives for the AI tooling era.
// Write an MCP server in 10 lines: declarative tool/resource/prompt
// registration, auto-detecting transport (stdio or HTTP+SSE), JSON-RPC
// framing handled by the runtime.
//
// Usage:
//   import { McpServer } from 'oam:mcp';
//
//   const server = new McpServer({ name: 'my-tools', version: '1.0.0' });
//
//   server.tool('get_weather', {
//     description: 'Get weather for a city',
//     parameters: { type: 'object', properties: { city: { type: 'string' } }, required: ['city'] },
//     handler: async ({ city }) => ({
//       content: [{ type: 'text', text: `Weather in ${city}: sunny, 72F` }],
//     }),
//   });
//
//   server.serve();

(function oamMcpModule(registry) {
  registry.factories["oam:mcp"] = () => {
    const PROTOCOL_VERSION = "2025-11-25";
    // Revisions this server can speak, newest first. Same list as the
    // `oam mcp` host in crates/oam_mcp. `initialize` answers the client's
    // revision when it is one of these and the newest otherwise, which is the
    // negotiation the spec describes.
    const SUPPORTED_VERSIONS = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
    const JSONRPC = "2.0";
    const SESSION_TIMEOUT_MS = 30 * 60 * 1000; // idle session reaper (SSE and /mcp)
    // Streamable-HTTP sessions are created by any `initialize`, so the table
    // is bounded: the least recently used entry goes when it is full.
    const MAX_HTTP_SESSIONS = 1024;
    // Largest request body the HTTP transports buffer before answering 413.
    const MAX_BODY_BYTES = 16 * 1024 * 1024;

    /** The JSON text of a response envelope, or null for no response. A
     *  result JSON.stringify cannot encode (a cycle, a BigInt, a throwing
     *  toJSON) is answered with -32603 under the request's own id. */
    function encodeResponse(response) {
      if (!response) return null;
      try {
        const s = JSON.stringify(response);
        if (typeof s === "string") return s;
        throw new Error("response did not serialise to JSON text");
      } catch (err) {
        return JSON.stringify({
          jsonrpc: JSONRPC,
          id: response.id === undefined ? null : response.id,
          error: {
            code: -32603,
            message: `result could not be serialised: ${String((err && err.message) || err)}`,
          },
        });
      }
    }

    /** Per-client protocol state. One per stdio process, one per SSE
     *  connection, one per Mcp-Session-Id on the streamable-HTTP endpoint --
     *  never one per server, or the first client to initialise would
     *  initialise everybody. */
    function newClientState() {
      return { initialized: false, protocolVersion: null, lastSeen: Date.now() };
    }

    /** JSON text for a handler result. `JSON.stringify` returns undefined (not
     *  a string) for undefined, a function or a symbol, and a `text` field
     *  holding undefined vanishes on serialisation, leaving a block that no
     *  schema accepts. */
    function jsonText(value) {
      const s = JSON.stringify(value);
      return typeof s === "string" ? s : "";
    }

    class McpServer {
      #name;
      #version;
      #tools = new Map();
      #resources = new Map();
      #resourceTemplates = new Map();
      #prompts = new Map();
      #onClose = null;

      constructor(options) {
        if (!options || !options.name) {
          throw new Error("McpServer requires { name }");
        }
        this.#name = options.name;
        this.#version = options.version || "0.0.0";
      }

      tool(name, config) {
        if (typeof name !== "string" || !name) {
          throw new Error("tool name must be a non-empty string");
        }
        if (!config || typeof config.handler !== "function") {
          throw new Error(`tool '${name}': handler is required and must be a function`);
        }
        this.#tools.set(name, {
          description: config.description || "",
          inputSchema: config.parameters || config.inputSchema || { type: "object", properties: {} },
          handler: config.handler,
        });
        return this;
      }

      resource(uri, config) {
        if (typeof uri !== "string" || !uri) {
          throw new Error("resource uri must be a non-empty string");
        }
        if (!config || typeof config.handler !== "function") {
          throw new Error(`resource '${uri}': handler is required and must be a function`);
        }
        this.#resources.set(uri, {
          name: config.name || uri,
          description: config.description || "",
          mimeType: config.mimeType || "text/plain",
          handler: config.handler,
        });
        return this;
      }

      resourceTemplate(uriTemplate, config) {
        if (typeof uriTemplate !== "string" || !uriTemplate) {
          throw new Error("resourceTemplate uriTemplate must be a non-empty string");
        }
        if (!config || typeof config.handler !== "function") {
          throw new Error(`resourceTemplate '${uriTemplate}': handler is required`);
        }
        // Compiled once, here, so a template that cannot become a regex (a
        // repeated placeholder name, or one that is not a valid group name
        // such as `{1}`) throws at registration instead of on every read.
        let pattern;
        try {
          pattern = compileUriTemplate(uriTemplate);
        } catch (err) {
          throw new Error(`resourceTemplate '${uriTemplate}': ${String((err && err.message) || err)}`);
        }
        this.#resourceTemplates.set(uriTemplate, {
          pattern,
          name: config.name || uriTemplate,
          description: config.description || "",
          mimeType: config.mimeType || "text/plain",
          handler: config.handler,
        });
        return this;
      }

      prompt(name, config) {
        if (typeof name !== "string" || !name) {
          throw new Error("prompt name must be a non-empty string");
        }
        if (!config || typeof config.handler !== "function") {
          throw new Error(`prompt '${name}': handler is required and must be a function`);
        }
        this.#prompts.set(name, {
          description: config.description || "",
          arguments: config.arguments || [],
          handler: config.handler,
        });
        return this;
      }

      /** stdio only: called once when the client closes the input stream.
       *  The HTTP transport never calls it; close the returned server. */
      onClose(fn) {
        this.#onClose = fn;
        return this;
      }

      async serve(options) {
        const opts = options || {};
        const transport = opts.transport || detectTransport();

        if (transport === "stdio") {
          return this.#serveStdio();
        }
        if (transport === "http") {
          // `??`, not `||`: port 0 is a real request (the OS picks a free
          // port, reported back on the resolved value), not "unset".
          const envPort = parseInt(process.env.PORT, 10);
          return this.#serveHttp(
            opts.port ?? (Number.isInteger(envPort) && envPort >= 0 ? envPort : 3000),
            opts.host || "127.0.0.1",
            opts.allowedOrigins,
          );
        }
        throw new Error(`unknown transport: ${transport}`);
      }

      #buildCapabilities() {
        const caps = {};
        if (this.#tools.size > 0) caps.tools = {};
        if (this.#resources.size > 0 || this.#resourceTemplates.size > 0) caps.resources = {};
        if (this.#prompts.size > 0) caps.prompts = {};
        return caps;
      }

      /** The JSON text to send back for `message`, or null when nothing is
       *  owed (a notification, or a batch of only notifications). Serialised
       *  here, per response, so a handler result JSON.stringify cannot encode
       *  becomes a -32603 carrying the request's own id rather than a throw in
       *  the transport, after the transport has lost track of the id. */
      async #handleMessage(message, client) {
        if (Array.isArray(message)) {
          // JSON-RPC 2.0: an empty batch is answered with one Invalid Request.
          if (message.length === 0) {
            return encodeResponse({
              jsonrpc: JSONRPC,
              id: null,
              error: { code: -32600, message: "invalid request: empty batch" },
            });
          }
          const parts = [];
          for (const msg of message) {
            const r = encodeResponse(await this.#handleSingle(msg, client));
            if (r !== null) parts.push(r);
          }
          return parts.length ? `[${parts.join(",")}]` : null;
        }
        return encodeResponse(await this.#handleSingle(message, client));
      }

      async #handleSingle(message, client) {
        // A JSON-RPC message must be an object. `JSON.parse` happily returns
        // null, a number or a string for a well-formed body, and reading `.id`
        // off null throws -- which on the HTTP transports lands in an
        // un-awaited async `createServer` callback and takes the whole runtime
        // down with OAM-RT0004. On the SSE path the 202 has already been sent,
        // so the client sees success against a server that is now dead.
        //
        // Guarding HERE rather than in each transport fixes stdio, SSE and
        // streamable-HTTP at once, and answers with the envelope the spec asks
        // for instead of a dropped connection.
        if (message === null || typeof message !== "object" || Array.isArray(message)) {
          return {
            jsonrpc: JSONRPC,
            id: null,
            error: { code: -32600, message: "invalid request: message must be an object" },
          };
        }

        const id = message.id;
        const method = message.method;
        const params = message.params || {};
        const isNotification = id === undefined || id === null;

        if (typeof method !== "string") {
          if (isNotification) return null;
          return { jsonrpc: JSONRPC, id, error: { code: -32600, message: "invalid request: method must be a string" } };
        }

        if (method === "notifications/initialized" ||
            method === "notifications/cancelled" ||
            method === "notifications/roots/list_changed") {
          return null;
        }

        if (isNotification) return null;

        // ping is exempt: the MCP lifecycle allows it before initialization,
        // and the `oam mcp` host answers it unconditionally too.
        if (method !== "initialize" && method !== "ping" && !client.initialized) {
          return { jsonrpc: JSONRPC, id, error: { code: -32002, message: "server not initialized" } };
        }

        let result;
        try {
          result = await this.#dispatch(method, params, client);
        } catch (err) {
          return { jsonrpc: JSONRPC, id, error: { code: -32603, message: String(err.message || err) } };
        }

        if (result && result.__jsonrpc_error) {
          return { jsonrpc: JSONRPC, id, error: result.__jsonrpc_error };
        }

        return { jsonrpc: JSONRPC, id, result };
      }

      async #dispatch(method, params, client) {
        switch (method) {
          case "initialize": {
            const requested = params.protocolVersion;
            client.protocolVersion =
              typeof requested === "string" && SUPPORTED_VERSIONS.includes(requested)
                ? requested
                : PROTOCOL_VERSION;
            client.initialized = true;
            return {
              protocolVersion: client.protocolVersion,
              capabilities: this.#buildCapabilities(),
              serverInfo: { name: this.#name, version: this.#version },
            };
          }

          case "ping":
            return {};

          case "tools/list":
            return { tools: this.#toolDefinitions() };

          case "tools/call":
            if (typeof params.name !== "string") {
              return { __jsonrpc_error: { code: -32602, message: "invalid params: name must be a string" } };
            }
            return this.#callTool(params.name, params.arguments || {});

          case "resources/list":
            return { resources: this.#resourceDefinitions() };

          case "resources/read":
            // Checked before lookup: a template match calls string methods on
            // the uri, so a missing one would otherwise surface as -32603.
            if (typeof params.uri !== "string") {
              return { __jsonrpc_error: { code: -32602, message: "invalid params: uri must be a string" } };
            }
            return this.#readResource(params.uri);

          case "resources/templates/list":
            return { resourceTemplates: this.#resourceTemplateDefinitions() };

          case "prompts/list":
            return { prompts: this.#promptDefinitions() };

          case "prompts/get":
            if (typeof params.name !== "string") {
              return { __jsonrpc_error: { code: -32602, message: "invalid params: name must be a string" } };
            }
            return this.#getPrompt(params.name, params.arguments || {});

          default:
            return { __jsonrpc_error: { code: -32601, message: `method not found: ${method}` } };
        }
      }

      #toolDefinitions() {
        const defs = [];
        for (const [name, t] of this.#tools) {
          defs.push({
            name,
            description: t.description,
            inputSchema: t.inputSchema,
          });
        }
        return defs;
      }

      async #callTool(name, args) {
        const tool = this.#tools.get(name);
        if (!tool) {
          return { __jsonrpc_error: { code: -32602, message: `unknown tool: ${name}` } };
        }
        try {
          const result = await tool.handler(args);
          if (result && result.content) return result;
          if (typeof result === "string") {
            return { content: [{ type: "text", text: result }] };
          }
          // A side-effect-only handler returns nothing: an empty content
          // array is the valid "no output" result.
          if (result === undefined) return { content: [] };
          return { content: [{ type: "text", text: jsonText(result) }] };
        } catch (err) {
          return {
            content: [{ type: "text", text: String(err.message || err) }],
            isError: true,
          };
        }
      }

      #resourceDefinitions() {
        const defs = [];
        for (const [uri, r] of this.#resources) {
          defs.push({ uri, name: r.name, description: r.description, mimeType: r.mimeType });
        }
        return defs;
      }

      async #readResource(uri) {
        const resource = this.#resources.get(uri);
        if (!resource) {
          for (const rt of this.#resourceTemplates.values()) {
            const params = matchUriTemplate(rt.pattern, uri);
            if (params) {
              const result = await rt.handler(uri, params);
              return formatResourceResult(uri, rt.mimeType, result);
            }
          }
          return { __jsonrpc_error: { code: -32602, message: `unknown resource: ${uri}` } };
        }
        const result = await resource.handler(uri);
        return formatResourceResult(uri, resource.mimeType, result);
      }

      #resourceTemplateDefinitions() {
        const defs = [];
        for (const [uriTemplate, rt] of this.#resourceTemplates) {
          defs.push({ uriTemplate, name: rt.name, description: rt.description, mimeType: rt.mimeType });
        }
        return defs;
      }

      #promptDefinitions() {
        const defs = [];
        for (const [name, p] of this.#prompts) {
          defs.push({ name, description: p.description, arguments: p.arguments });
        }
        return defs;
      }

      async #getPrompt(name, args) {
        const prompt = this.#prompts.get(name);
        if (!prompt) {
          return { __jsonrpc_error: { code: -32602, message: `unknown prompt: ${name}` } };
        }
        const result = await prompt.handler(args);
        if (result && result.messages) return result;
        if (typeof result === "string") {
          return { messages: [{ role: "user", content: { type: "text", text: result } }] };
        }
        if (Array.isArray(result)) {
          return { messages: result };
        }
        if (result === undefined) return { messages: [] };
        return { messages: [{ role: "user", content: { type: "text", text: jsonText(result) } }] };
      }

      // ---- stdio transport ----

      async #serveStdio() {
        const stdin = process.stdin;
        const stdout = process.stdout;
        let buffer = "";
        let chain = Promise.resolve();
        const client = newClientState();
        // One decoder for the whole stream, so a multi-byte character split
        // across two chunks is reassembled rather than replaced.
        const decoder = new TextDecoder();

        const processLine = async (line) => {
          line = line.trim();
          if (!line) return;

          let message;
          try {
            message = JSON.parse(line);
          } catch {
            const err = { jsonrpc: JSONRPC, id: null, error: { code: -32700, message: "parse error" } };
            stdout.write(JSON.stringify(err) + "\n");
            return;
          }

          const response = await this.#handleMessage(message, client);
          if (response !== null) {
            stdout.write(response + "\n");
          }
        };

        // Lines are handled one at a time, in arrival order: a request is not
        // dispatched until the previous one has been answered. (The HTTP
        // transports run requests concurrently.)
        const queueLine = (line) => {
          chain = chain
            .then(() => processLine(line))
            .catch((err) => {
              // processLine catches handler, parse and serialisation errors
              // itself; this guards anything else (a throwing stdout.write)
              // so the client gets a frame, not silence.
              try {
                stdout.write(
                  JSON.stringify({
                    jsonrpc: JSONRPC,
                    id: null,
                    error: { code: -32603, message: String((err && err.message) || err) },
                  }) + "\n",
                );
              } catch {}
            });
        };

        return new Promise((resolve) => {
          const onData = (chunk) => {
            buffer += (typeof chunk === "string") ? chunk : decoder.decode(chunk, { stream: true });
            const lines = buffer.split("\n");
            buffer = lines.pop();
            for (const line of lines) queueLine(line);
          };

          // Registered for both 'end' and 'error'; runs once, whichever
          // comes first.
          let ended = false;
          const onEnd = () => {
            if (ended) return;
            ended = true;
            stdin.removeListener("data", onData);
            // A last message the client sent without a trailing newline is
            // still a message: handle it rather than drop it.
            const tail = buffer + decoder.decode();
            buffer = "";
            if (tail.trim()) queueLine(tail);
            chain.then(() => {
              if (this.#onClose) {
                try { this.#onClose(); } catch {}
              }
              resolve();
            });
          };

          stdin.setEncoding("utf8");
          stdin.on("data", onData);
          stdin.on("end", onEnd);
          stdin.on("error", onEnd);
          stdin.resume();
        });
      }

      // ---- HTTP + SSE transport ----

      async #serveHttp(port, host, allowedOrigins) {
        const { createServer } = __oamNode.get("http");
        const sessions = new Map();
        // Streamable-HTTP client state, keyed by Mcp-Session-Id.
        const httpSessions = new Map();
        const allowed = normalizeOrigins(allowedOrigins);

        const server = createServer(async (req, res) => {
          try {
            // ORIGIN GATE, before any routing.
            //
            // This server binds 127.0.0.1 and detectTransport() picks HTTP
            // whenever stdin is a TTY -- so a plain interactive `oam run
            // server.js` is listening on localhost by default. "Only local"
            // is not a boundary against a BROWSER: any page the developer
            // visits can POST here, and a CORS-simple request does not need
            // preflight, so the tool call lands even though the page
            // cannot read the reply. Blind is not harmless -- MCP tools have
            // side effects, which is the point of them.
            //
            // The gate reads Origin only; the Host header is not checked.
            // It therefore covers the requests a browser sends with an
            // Origin -- every POST and DELETE, so tool calls and session
            // messages from a page under any other name (one that resolves
            // to 127.0.0.1 included) are refused. A GET can arrive with no
            // Origin (a same-origin fetch, or a no-cors load such as an
            // <img>), so GET /sse and GET /health answer any page.
            //
            // A request with NO Origin header is allowed: that is every
            // non-browser client (a real MCP client, curl, a test). Browsers
            // always send Origin on a POST or DELETE, so the absence of one
            // there is not something a page can arrange.
            const origin = req.headers.origin;
            if (typeof origin === "string" && origin !== "" && !allowed.has(origin.toLowerCase())) {
              res.writeHead(403, { "Content-Type": "application/json" });
              res.end(
                JSON.stringify({
                  jsonrpc: JSONRPC,
                  id: null,
                  error: {
                    code: -32600,
                    message:
                      "forbidden origin: pass allowedOrigins to serve() to permit a browser origin",
                  },
                }),
              );
              return;
            }

            // A fixed base: only the path and query are read, and a Host
            // header that is not a valid authority must not make the parse
            // throw (which would answer 500 for what is client input).
            const url = new URL(req.url, "http://localhost");

            if (req.method === "GET" && url.pathname === "/sse") {
              return this.#handleSseConnect(req, res, sessions, crypto.randomUUID());
            }
            if (req.method === "POST" && url.pathname === "/message") {
              return await this.#handleSseMessage(req, res, sessions, url);
            }
            if (req.method === "POST" && url.pathname === "/mcp") {
              return await this.#handleStreamableHttp(req, res, httpSessions);
            }
            if (req.method === "DELETE" && url.pathname === "/mcp") {
              // The client ending its session, per the streamable-HTTP spec.
              const sid = req.headers["mcp-session-id"];
              if (typeof sid !== "string" || sid === "") {
                res.writeHead(400);
                res.end("Mcp-Session-Id header required");
              } else if (httpSessions.delete(sid)) {
                res.writeHead(204);
                res.end();
              } else {
                res.writeHead(404);
                res.end("session not found");
              }
              return;
            }
            if (req.method === "GET" && url.pathname === "/health") {
              res.writeHead(200, { "Content-Type": "application/json" });
              res.end(JSON.stringify({ status: "ok", server: this.#name, version: this.#version }));
              return;
            }

            res.writeHead(404);
            res.end("not found");
          } catch (err) {
            // Nothing may escape this callback. It is async and nobody awaits
            // it, so an unhandled rejection here is a dead runtime rather than
            // a failed request. The handlers write their own responses, hence
            // the headersSent check before writing another.
            try {
              process.stderr.write(`oam:mcp request failed: ${String((err && err.message) || err)}\n`);
            } catch {}
            try {
              if (!res.headersSent && err && err.code === "OAM_MCP_BODY_TOO_LARGE") {
                // The rest of the upload is discarded by readBody, and the
                // connection is not reused.
                res.writeHead(413, { "Content-Type": "application/json", Connection: "close" });
                res.end(
                  JSON.stringify({
                    jsonrpc: JSONRPC,
                    id: null,
                    error: { code: -32600, message: "request body too large" },
                  }),
                );
              } else if (!res.headersSent) {
                res.writeHead(500, { "Content-Type": "application/json" });
                res.end(
                  JSON.stringify({
                    jsonrpc: JSONRPC,
                    id: null,
                    error: { code: -32603, message: "internal error" },
                  }),
                );
              } else {
                res.end();
              }
            } catch {}
          }
        });

        return new Promise((resolve, reject) => {
          // `reject` covers the listen attempt only. Once listening, a later
          // server error would settle nothing, so it is logged instead.
          server.once("error", reject);
          server.listen(port, host, () => {
            server.removeListener("error", reject);
            server.on("error", (err) => {
              try {
                process.stderr.write(`oam:mcp server error: ${String((err && err.message) || err)}\n`);
              } catch {}
            });
            const addr = server.address();
            process.stderr.write(`oam:mcp server listening on ${addr.address}:${addr.port}\n`);
            resolve({ server, port: addr.port, host: addr.address });
          });
        });
      }

      #handleSseConnect(req, res, sessions, sessionId) {
        res.writeHead(200, {
          "Content-Type": "text/event-stream",
          "Cache-Control": "no-cache",
          Connection: "keep-alive",
        });

        const endpoint = `/message?sessionId=${sessionId}`;
        res.write(`event: endpoint\ndata: ${endpoint}\n\n`);

        const session = { res, alive: true, timer: null, client: newClientState() };
        session.timer = setTimeout(() => {
          session.alive = false;
          sessions.delete(sessionId);
          try { res.end(); } catch {}
        }, SESSION_TIMEOUT_MS);

        sessions.set(sessionId, session);

        req.on("close", () => {
          // session.timer, not the timer armed above: every message re-arms
          // it, so the one captured at connect time is long gone by now.
          // `alive` goes false so a reply still in flight is dropped rather
          // than written to an ended response.
          session.alive = false;
          clearTimeout(session.timer);
          session.timer = null;
          sessions.delete(sessionId);
        });
      }

      async #handleSseMessage(req, res, sessions, url) {
        // A string, verbatim. The id used to be an incrementing integer, which
        // a caller who reached this endpoint could simply guess -- and the
        // `parseInt` meant `?sessionId=1abc` resolved to session 1. It is a
        // randomUUID now, so possession of the id is worth something.
        const sessionId = url.searchParams.get("sessionId") || "";
        const session = sessions.get(sessionId);
        if (!session) {
          res.writeHead(404);
          res.end("session not found");
          return;
        }

        const body = await readBody(req);
        let message;
        try {
          message = JSON.parse(body);
        } catch {
          res.writeHead(400);
          res.end("invalid JSON");
          return;
        }

        res.writeHead(202);
        res.end("accepted");

        // Re-arm only while the stream is open: the body read above awaits,
        // and the client may have disconnected in the meantime -- arming a
        // timer then would hold the ended response for the full timeout.
        if (session.alive && session.timer) {
          clearTimeout(session.timer);
          session.timer = setTimeout(() => {
            session.alive = false;
            sessions.delete(sessionId);
            try { session.res.end(); } catch {}
          }, SESSION_TIMEOUT_MS);
        }

        const response = await this.#handleMessage(message, session.client);
        if (response !== null && session.alive) {
          session.res.write(`event: message\ndata: ${response}\n\n`);
        }
      }

      async #handleStreamableHttp(req, res, httpSessions) {
        const body = await readBody(req);
        let message;
        try {
          message = JSON.parse(body);
        } catch {
          res.writeHead(400, { "Content-Type": "application/json" });
          res.end(JSON.stringify({ jsonrpc: JSONRPC, id: null, error: { code: -32700, message: "parse error" } }));
          return;
        }

        // Protocol state belongs to the session the request names. A request
        // with no Mcp-Session-Id gets a fresh, uninitialised state: it can
        // `initialize` (which mints a session and returns its id in the
        // response header) and nothing else. An id this server did not issue,
        // or one that has expired, is 404 -- the spec's signal to the client
        // to start a new session.
        const headerId = req.headers["mcp-session-id"];
        let sessionId = null;
        let client;
        if (typeof headerId === "string" && headerId !== "") {
          client = httpSessions.get(headerId);
          if (client && Date.now() - client.lastSeen > SESSION_TIMEOUT_MS) {
            httpSessions.delete(headerId);
            client = undefined;
          }
          if (!client) {
            res.writeHead(404, { "Content-Type": "application/json" });
            res.end(JSON.stringify({ jsonrpc: JSONRPC, id: null, error: { code: -32001, message: "session not found" } }));
            return;
          }
          sessionId = headerId;
          client.lastSeen = Date.now();
          // Re-insert so Map order is least-recently-used first, which is
          // the order the cap below evicts in.
          httpSessions.delete(headerId);
          httpSessions.set(headerId, client);
        } else {
          client = newClientState();
        }

        const response = await this.#handleMessage(message, client);

        if (sessionId === null && client.initialized) {
          const now = Date.now();
          for (const [id, s] of httpSessions) {
            if (now - s.lastSeen > SESSION_TIMEOUT_MS) httpSessions.delete(id);
          }
          while (httpSessions.size >= MAX_HTTP_SESSIONS) {
            httpSessions.delete(httpSessions.keys().next().value);
          }
          sessionId = crypto.randomUUID();
          client.lastSeen = now;
          httpSessions.set(sessionId, client);
        }

        const headers = { "Content-Type": "application/json" };
        if (sessionId !== null) headers["Mcp-Session-Id"] = sessionId;
        if (response !== null) {
          res.writeHead(200, headers);
          res.end(response);
        } else {
          if (sessionId !== null) {
            res.writeHead(202, { "Mcp-Session-Id": sessionId });
          } else {
            res.writeHead(202);
          }
          res.end();
        }
      }
    }

    // ---- helpers ----

    /** Build the allowed-origin set for the HTTP transport.
     *
     *  Empty by default: no browser origin is trusted unless the embedder
     *  names one. Compared lowercased, since Origin is case-insensitive in
     *  scheme and host. */
    function normalizeOrigins(list) {
      const out = new Set();
      if (!list) return out;
      const items = Array.isArray(list) ? list : [list];
      for (const item of items) {
        if (typeof item === "string" && item !== "") out.add(item.toLowerCase());
      }
      return out;
    }

    // stdio whenever stdin is not a terminal -- which is how an MCP client
    // spawns a server (stdin is a pipe). The same test is true of a service
    // or container started with stdin closed or redirected: that process
    // serves stdio, reads EOF and returns. A server meant to listen there
    // must say so with serve({ transport: "http" }); the environment is not
    // consulted, since a stdio server routinely inherits an unrelated PORT.
    function detectTransport() {
      try {
        if (process.stdin && !process.stdin.isTTY) return "stdio";
      } catch {}
      return "http";
    }

    function readBody(req) {
      return new Promise((resolve, reject) => {
        // One streaming decoder for the whole body: a UTF-8 sequence that
        // straddles a chunk boundary is carried over, not turned into two
        // replacement characters. Bounded, because the body is buffered whole.
        const decoder = new TextDecoder();
        const chunks = [];
        let size = 0;
        let overflowed = false;
        req.on("data", (c) => {
          if (overflowed) return; // keep draining, keep nothing
          size += c.length;
          if (size > MAX_BODY_BYTES) {
            overflowed = true;
            chunks.length = 0;
            const err = new Error(`request body exceeds ${MAX_BODY_BYTES} bytes`);
            err.code = "OAM_MCP_BODY_TOO_LARGE";
            reject(err);
            return;
          }
          chunks.push(typeof c === "string" ? c : decoder.decode(c, { stream: true }));
        });
        req.on("end", () => {
          if (overflowed) return;
          chunks.push(decoder.decode());
          resolve(chunks.join(""));
        });
        req.on("error", reject);
      });
    }

    function formatResourceResult(uri, mimeType, result) {
      if (typeof result === "string") {
        return { contents: [{ uri, mimeType, text: result }] };
      }
      if (result instanceof Uint8Array || (result && result.constructor === ArrayBuffer)) {
        const bytes = result instanceof Uint8Array ? result : new Uint8Array(result);
        const binary = [];
        for (let i = 0; i < bytes.length; i++) binary.push(String.fromCharCode(bytes[i]));
        return { contents: [{ uri, mimeType, blob: btoa(binary.join("")) }] };
      }
      if (result && result.contents) return result;
      // jsonText: an undefined result reads as an empty resource, never as a
      // contents entry with neither `text` nor `blob`.
      return { contents: [{ uri, mimeType, text: jsonText(result) }] };
    }

    /** The anchored regex for a resource template. Each `{name}` becomes a
     *  named group matching one path segment; `new RegExp` throws for a
     *  repeated or invalid group name. */
    function compileUriTemplate(template) {
      // Split on {param} placeholders; escape metacharacters only in static segments.
      const parts = template.split(/(\{\w+\})/);
      const regexStr = parts.map((part, i) =>
        i % 2 === 0
          ? part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")
          : part.replace(/\{(\w+)\}/, "(?<$1>[^/]+)")
      ).join("");
      return new RegExp(`^${regexStr}$`);
    }

    /** The template's parameters for `uri`, or null when it does not match.
     *  Values are the matched text as it appears in the uri: percent-escapes
     *  are not decoded, so a handler that wants decoded values calls
     *  decodeURIComponent itself. */
    function matchUriTemplate(pattern, uri) {
      const match = uri.match(pattern);
      return match ? match.groups || {} : null;
    }

    return {
      McpServer,
      PROTOCOL_VERSION,
    };
  };
})(globalThis.__oamNode);
