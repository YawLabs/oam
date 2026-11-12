// oam:ai -- AI/LLM helpers for streaming chat, SSE parsing, and tool-use
// loops. Pure JS over fetch + ReadableStream; no native ops required.
//
// Usage:
//   import { parseSSEStream, streamChat, runToolLoop } from 'oam:ai';

(function oamAiModule(registry) {
  registry.factories["oam:ai"] = () => {
    // --------------------------------------------------------- SSE parser
    // Parses a ReadableStream of bytes into an async iterable of SSE events.
    // Handles multi-line data fields, event/id fields, and comment lines.
    async function* parseSSEStream(readableStream) {
      const decoder = new TextDecoder();
      let buffer = "";
      let event = "";
      let data = "";
      let id = "";

      // Feeds one complete line (terminator already removed). Returns the
      // event a blank line dispatches, or null.
      function feedLine(line) {
        if (line === "") {
          // Empty line = event boundary.
          const out = data
            ? {
                event: event || "message",
                data: data.endsWith("\n") ? data.slice(0, -1) : data,
                id,
              }
            : null;
          event = "";
          data = "";
          id = "";
          return out;
        }
        if (line.startsWith(":")) return null; // comment

        const colon = line.indexOf(":");
        let field, value2;
        if (colon === -1) {
          field = line;
          value2 = "";
        } else {
          field = line.slice(0, colon);
          value2 = line.slice(colon + 1);
          if (value2.startsWith(" ")) value2 = value2.slice(1);
        }

        switch (field) {
          case "event":
            event = value2;
            break;
          case "data":
            data += value2 + "\n";
            break;
          case "id":
            id = value2;
            break;
          // "retry" is intentionally ignored (no reconnection in this API).
        }
        return null;
      }

      const reader = readableStream.getReader();
      // True once the source reported done. Any other way out of this
      // generator (the consumer broke out of its loop, a read threw) leaves
      // the body unread, and it has to be cancelled, not just unlocked.
      let finished = false;
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });

          // The SSE grammar ends a line with CRLF, a lone LF, or a lone CR.
          // A CR that is the last character read so far may be the first
          // half of a CRLF the next chunk completes, so it stays in the
          // buffer (with its line) until more bytes -- or the end -- arrive.
          let ready = buffer;
          let held = "";
          if (ready.endsWith("\r")) {
            held = "\r";
            ready = ready.slice(0, -1);
          }
          const lines = ready.split(/\r\n|\r|\n/);
          // Last element is incomplete (no terminator yet) -- keep it.
          buffer = lines.pop() + held;

          for (const line of lines) {
            const out = feedLine(line);
            if (out) yield out;
          }
        }
        finished = true;
        // A line whose CR terminator was the last byte of the stream is a
        // complete line; an unterminated tail is not, and stays dropped.
        if (buffer.endsWith("\r")) {
          const tail = buffer.slice(0, -1);
          buffer = "";
          for (const line of tail.split(/\r\n|\r|\n/)) {
            const out = feedLine(line);
            if (out) yield out;
          }
        }
        // Flush trailing event if stream ended without a final empty line.
        if (data) {
          const out = {
            event: event || "message",
            data: data.endsWith("\n") ? data.slice(0, -1) : data,
            id,
          };
          data = "";
          yield out;
        }
      } finally {
        if (!finished) {
          // Early exit: release the underlying response body rather than
          // leave it open. A cancel that rejects (the stream already
          // errored) must not replace the error on its way out.
          try {
            await reader.cancel();
          } catch {
            /* already errored or closed */
          }
        }
        reader.releaseLock();
      }
    }

    // ----------------------------------------------- streaming chat helper
    // Wraps fetch + SSE parse for the common "POST JSON, stream SSE back"
    // pattern every LLM API uses. Yields content delta strings.
    //
    //   for await (const chunk of streamChat({ url, headers, body })) {
    //     process.stdout.write(chunk);
    //   }
    async function* streamChat({ url, headers, body, signal, extractDelta }) {
      const response = await fetch(url, {
        method: "POST",
        headers: { "Content-Type": "application/json", ...headers },
        body: typeof body === "string" ? body : JSON.stringify(body),
        signal,
      });
      if (!response.ok) {
        const text = await response.text().catch(() => "");
        throw new Error(
          `streamChat: ${response.status} ${response.statusText}${text ? " -- " + text : ""}`
        );
      }
      const extract = extractDelta || defaultExtractDelta;
      for await (const event of parseSSEStream(response.body)) {
        if (event.data === "[DONE]") return;
        let parsed;
        let isJson = true;
        try {
          parsed = JSON.parse(event.data);
        } catch {
          isJson = false;
        }
        // A provider reports a mid-stream failure as a frame, not a status:
        // Anthropic sends `event: error` with { type: "error", error }, and
        // OpenAI-shaped servers send a bare { error }. Neither carries a
        // delta, so skipping them would end the stream looking complete.
        const errorBody =
          isJson && parsed !== null && typeof parsed === "object" ? parsed.error : undefined;
        if (
          event.event === "error" ||
          (isJson && parsed !== null && typeof parsed === "object" && parsed.type === "error") ||
          (errorBody !== null &&
            typeof errorBody === "object" &&
            parsed.choices === undefined &&
            parsed.delta === undefined)
        ) {
          const detail =
            (errorBody && typeof errorBody === "object" && errorBody.message) ||
            (typeof errorBody === "string" && errorBody) ||
            event.data;
          const err = new Error(`streamChat: stream error -- ${detail}`);
          err.data = isJson ? parsed : event.data;
          throw err;
        }
        if (!isJson) continue; // non-JSON SSE line (keepalive, etc.)
        const delta = extract(parsed, event);
        if (delta != null) yield delta;
      }
    }

    // Default delta extractor: covers OpenAI and Anthropic shapes.
    function defaultExtractDelta(parsed) {
      // OpenAI: choices[0].delta.content
      const oai = parsed?.choices?.[0]?.delta?.content;
      if (oai != null) return oai;
      // Anthropic: delta.text (content_block_delta)
      const anth = parsed?.delta?.text;
      if (anth != null) return anth;
      return null;
    }

    // ------------------------------------------------- provider presets
    // Convenience wrappers that set the right URL + auth header shape.

    function openai(apiKey, baseURL) {
      const base = baseURL || "https://api.openai.com/v1";
      return {
        chat(messages, options) {
          const { model = "gpt-4o", signal, ...rest } = options || {};
          return streamChat({
            url: `${base}/chat/completions`,
            headers: { Authorization: `Bearer ${apiKey}` },
            body: { model, messages, stream: true, ...rest },
            signal,
          });
        },
      };
    }

    function anthropic(apiKey, baseURL) {
      const base = baseURL || "https://api.anthropic.com/v1";
      return {
        chat(messages, options) {
          const {
            model = "claude-sonnet-5-5",
            max_tokens = 4096,
            system,
            signal,
            ...rest
          } = options || {};
          const body = { model, messages, max_tokens, stream: true, ...rest };
          if (system) body.system = system;
          return streamChat({
            url: `${base}/messages`,
            headers: {
              "x-api-key": apiKey,
              "anthropic-version": "2023-06-01",
            },
            body,
            signal,
            extractDelta(parsed) {
              return parsed?.delta?.text ?? null;
            },
          });
        },
      };
    }

    // --------------------------------------------------- tool-use loop
    // Runs the call-tool-respond cycle for agentic workloads.
    //
    // `chat` must return a WHOLE response, not a stream. The presets above are
    // deliberately NOT drop-ins: openai().chat and anthropic().chat are async
    // generators, and awaiting a generator hands back the generator itself --
    // which has no `.content` and no `.choices`, so the loop would find no
    // tool calls, return { text: '', iterations: 1 }, and never issue a single
    // request. This example used to pass `anthropic(key).chat` and did exactly
    // that, silently. The TypeError below now catches it; oam.d.ts has always
    // said so in prose.
    //
    //   const result = await runToolLoop({
    //     chat: async (messages, options) => {
    //       const res = await fetch('https://api.anthropic.com/v1/messages', {
    //         method: 'POST',
    //         headers: {
    //           'content-type': 'application/json',
    //           'x-api-key': key,
    //           'anthropic-version': '2023-06-01',
    //         },
    //         // options carries stream:false plus any tools the loop added.
    //         body: JSON.stringify({
    //           model: 'claude-sonnet-5-5',
    //           max_tokens: 4096,
    //           messages,
    //           ...options,
    //         }),
    //       });
    //       return res.json();
    //     },
    //     tools: [{ name: 'get_weather', ... }],
    //     messages: [{ role: 'user', content: 'What is the weather?' }],
    //     onToolCall: async ({ name, input }) => JSON.stringify({ temp: 72 }),
    //   });
    async function runToolLoop({
      chat,
      tools,
      messages,
      onToolCall,
      onDelta,
      maxIterations = 10,
      chatOptions = {},
    }) {
      const msgs = [...messages];
      for (let i = 0; i < maxIterations; i++) {
        // Collect the full response (non-streaming for tool detection).
        const options = { ...chatOptions, stream: false };
        if (tools?.length) options.tools = tools;

        const response = await chat(msgs, options);

        // A streaming `chat` is the one mistake this signature invites, and it
        // fails silently: an async generator is not a thenable, so `await`
        // returns the generator untouched, every shape test below misses, and
        // the caller gets an empty result having made no request at all. Say
        // so instead.
        if (response && typeof response[Symbol.asyncIterator] === "function") {
          throw new TypeError(
            "runToolLoop: `chat` returned an async iterable (a stream). It must return a " +
              "whole response -- the chat() from openai() / anthropic() streams and is not a " +
              "drop-in here; pass a non-streaming call of your own.",
          );
        }

        // For non-streaming responses, chat() returns the raw response.
        // Detect tool_use in Anthropic or tool_calls in OpenAI shape.
        let toolCalls = null;
        let textContent = "";

        if (response?.content) {
          // Anthropic shape: response.content is an array of blocks
          for (const block of response.content) {
            if (block.type === "tool_use") {
              toolCalls = toolCalls || [];
              toolCalls.push({
                id: block.id,
                name: block.name,
                input: block.input,
              });
            } else if (block.type === "text") {
              textContent += block.text;
              if (onDelta) onDelta(block.text);
            }
          }
        } else if (response?.choices?.[0]) {
          // OpenAI shape
          const choice = response.choices[0];
          textContent = choice.message?.content || "";
          if (onDelta && textContent) onDelta(textContent);
          if (choice.message?.tool_calls) {
            toolCalls = choice.message.tool_calls.map((tc) => ({
              id: tc.id,
              name: tc.function.name,
              // A no-argument call may carry an empty arguments string.
              input: tc.function.arguments ? JSON.parse(tc.function.arguments) : {},
            }));
          }
        }

        if (!toolCalls || toolCalls.length === 0) {
          return { text: textContent, response, iterations: i + 1 };
        }

        // Execute tool calls and append results.
        // The reply has to be in the shape of the conversation it answers:
        // which branch above parsed the response decides it, not a second
        // look at the response.
        const openaiShape = !response?.content;
        msgs.push(
          openaiShape
            ? response.choices[0].message
            : { role: "assistant", content: response.content },
        );
        const anthropicResults = [];
        for (const call of toolCalls) {
          const result = await onToolCall(call);
          const content =
            typeof result === "string" ? result : (JSON.stringify(result) ?? "");
          if (openaiShape) {
            // Chat Completions: one role:"tool" message per tool_calls entry.
            msgs.push({ role: "tool", tool_call_id: call.id, content });
          } else {
            anthropicResults.push({
              type: "tool_result",
              tool_use_id: call.id,
              content,
            });
          }
        }
        // Messages API: every tool_result of one assistant turn goes back in
        // a single user message.
        if (!openaiShape) msgs.push({ role: "user", content: anthropicResults });
      }
      return {
        text: "",
        response: null,
        iterations: maxIterations,
        exhausted: true,
      };
    }

    return {
      parseSSEStream,
      streamChat,
      openai,
      anthropic,
      runToolLoop,
      defaultExtractDelta,
    };
  };
})(globalThis.__oamNode);
