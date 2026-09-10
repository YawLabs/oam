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
    const JSONRPC = "2.0";
    const SESSION_TIMEOUT_MS = 30 * 60 * 1000; // idle SSE session reaper

    class McpServer {
      #name;
      #version;
      #tools = new Map();
      #resources = new Map();
      #resourceTemplates = new Map();
      #prompts = new Map();
      #initialized = false;
      #sessionId = null;
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
        this.#resourceTemplates.set(uriTemplate, {
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
          return this.#serveHttp(
            opts.port || parseInt(process.env.PORT, 10) || 3000,
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

      async #handleMessage(message) {
        if (Array.isArray(message)) {
          const responses = [];
          for (const msg of message) {
            const r = await this.#handleSingle(msg);
            if (r) responses.push(r);
          }
          return responses.length ? responses : null;
        }
        return this.#handleSingle(message);
      }

      async #handleSingle(message) {
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

        if (method !== "initialize" && !this.#initialized) {
          return { jsonrpc: JSONRPC, id, error: { code: -32002, message: "server not initialized" } };
        }

        let result;
        try {
          result = await this.#dispatch(method, params);
        } catch (err) {
          return { jsonrpc: JSONRPC, id, error: { code: -32603, message: String(err.message || err) } };
        }

        if (result && result.__jsonrpc_error) {
          return { jsonrpc: JSONRPC, id, error: result.__jsonrpc_error };
        }

        return { jsonrpc: JSONRPC, id, result };
      }

      async #dispatch(method, params) {
        switch (method) {
          case "initialize": {
            this.#initialized = true;
            this.#sessionId = crypto.randomUUID();
            return {
              protocolVersion: PROTOCOL_VERSION,
              capabilities: this.#buildCapabilities(),
              serverInfo: { name: this.#name, version: this.#version },
            };
          }

          case "ping":
            return {};

          case "tools/list":
            return { tools: this.#toolDefinitions() };

          case "tools/call":
            return this.#callTool(params.name, params.arguments || {});

          case "resources/list":
            return { resources: this.#resourceDefinitions() };

          case "resources/read":
            return this.#readResource(params.uri);

          case "resources/templates/list":
            return { resourceTemplates: this.#resourceTemplateDefinitions() };

          case "prompts/list":
            return { prompts: this.#promptDefinitions() };

          case "prompts/get":
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
          return { content: [{ type: "text", text: JSON.stringify(result) }] };
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
          for (const [template, rt] of this.#resourceTemplates) {
            const params = matchUriTemplate(template, uri);
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
        return { messages: [{ role: "user", content: { type: "text", text: JSON.stringify(result) } }] };
      }

      // ---- stdio transport ----

      async #serveStdio() {
        const stdin = process.stdin;
        const stdout = process.stdout;
        let buffer = "";
        let chain = Promise.resolve();

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

          const response = await this.#handleMessage(message);
          if (response) {
            stdout.write(JSON.stringify(response) + "\n");
          }
        };

        return new Promise((resolve) => {
          const onData = (chunk) => {
            buffer += (typeof chunk === "string") ? chunk : new TextDecoder().decode(chunk);
            const lines = buffer.split("\n");
            buffer = lines.pop();
            for (const line of lines) {
              chain = chain
                .then(() => processLine(line))
                .catch((err) => {
                  // processLine catches handler/parse errors itself; this
                  // guards a top-level blow-up (e.g. JSON.stringify on a
                  // circular handler result) so the client gets a frame,
                  // not silence.
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
            }
          };

          const onEnd = () => {
            chain.then(() => {
              if (this.#onClose) {
                try { this.#onClose(); } catch {}
              }
              resolve();
            });
          };

          if (stdin.on) {
            stdin.setEncoding && stdin.setEncoding("utf8");
            stdin.on("data", onData);
            stdin.on("end", onEnd);
            stdin.on("error", onEnd);
            if (stdin.resume) stdin.resume();
          } else {
            (async () => {
              try {
                for await (const chunk of stdin) {
                  onData(chunk);
                }
              } catch {}
              onEnd();
            })();
          }
        });
      }

      // ---- HTTP + SSE transport ----

      async #serveHttp(port, host, allowedOrigins) {
        const { createServer } = __oamNode.get("http");
        const sessions = new Map();
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
            // preflight, so the tool call lands even though the attacker
            // cannot read the reply. Blind is not harmless -- MCP tools have
            // side effects, which is the point of them. The same check also
            // blocks DNS rebinding, where the Host header is the attacker's
            // own name resolved to 127.0.0.1.
            //
            // A request with NO Origin header is allowed: that is every
            // non-browser client (a real MCP client, curl, a test). Browsers
            // always send Origin on a cross-origin request, so the absence of
            // one is not something a page can arrange.
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

            const url = new URL(req.url, `http://${req.headers.host || "localhost"}`);

            if (req.method === "GET" && url.pathname === "/sse") {
              return this.#handleSseConnect(req, res, sessions, crypto.randomUUID());
            }
            if (req.method === "POST" && url.pathname === "/message") {
              return await this.#handleSseMessage(req, res, sessions, url);
            }
            if (req.method === "POST" && url.pathname === "/mcp") {
              return await this.#handleStreamableHttp(req, res);
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
              if (!res.headersSent) {
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
          server.listen(port, host, () => {
            const addr = server.address();
            process.stderr.write(`oam:mcp server listening on ${addr.address}:${addr.port}\n`);
            resolve({ server, port: addr.port, host: addr.address });
          });
          server.on("error", reject);
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

        const timer = setTimeout(() => {
          sessions.delete(sessionId);
          try { res.end(); } catch {}
        }, SESSION_TIMEOUT_MS);

        sessions.set(sessionId, { res, alive: true, timer });

        req.on("close", () => {
          clearTimeout(timer);
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

        if (session.timer) {
          clearTimeout(session.timer);
          session.timer = setTimeout(() => {
            sessions.delete(sessionId);
            try { session.res.end(); } catch {}
          }, SESSION_TIMEOUT_MS);
        }

        const response = await this.#handleMessage(message);
        if (response && session.alive) {
          session.res.write(`event: message\ndata: ${JSON.stringify(response)}\n\n`);
        }
      }

      async #handleStreamableHttp(req, res) {
        const body = await readBody(req);
        let message;
        try {
          message = JSON.parse(body);
        } catch {
          res.writeHead(400, { "Content-Type": "application/json" });
          res.end(JSON.stringify({ jsonrpc: JSONRPC, id: null, error: { code: -32700, message: "parse error" } }));
          return;
        }

        const response = await this.#handleMessage(message);
        const headers = { "Content-Type": "application/json" };
        if (this.#sessionId) headers["Mcp-Session-Id"] = this.#sessionId;
        if (response) {
          res.writeHead(200, headers);
          res.end(JSON.stringify(response));
        } else {
          res.writeHead(202);
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

    function detectTransport() {
      try {
        if (process.stdin && !process.stdin.isTTY) return "stdio";
      } catch {}
      return "http";
    }

    function readBody(req) {
      return new Promise((resolve, reject) => {
        const chunks = [];
        req.on("data", (c) => chunks.push(typeof c === "string" ? c : new TextDecoder().decode(c)));
        req.on("end", () => resolve(chunks.join("")));
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
      return { contents: [{ uri, mimeType, text: JSON.stringify(result) }] };
    }

    function matchUriTemplate(template, uri) {
      // Split on {param} placeholders; escape metacharacters only in static segments.
      const parts = template.split(/(\{\w+\})/);
      const regexStr = parts.map((part, i) =>
        i % 2 === 0
          ? part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")
          : part.replace(/\{(\w+)\}/, "(?<$1>[^/]+)")
      ).join("");
      const match = uri.match(new RegExp(`^${regexStr}$`));
      return match ? match.groups || {} : null;
    }

    return {
      McpServer,
      PROTOCOL_VERSION,
    };
  };
})(globalThis.__oamNode);
