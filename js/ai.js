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

      const reader = readableStream.getReader();
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });

          const lines = buffer.split("\n");
          // Last element is incomplete (no trailing newline yet) -- keep it.
          buffer = lines.pop();

          for (const line of lines) {
            if (line === "") {
              // Empty line = event boundary.
              if (data) {
                yield {
                  event: event || "message",
                  data: data.endsWith("\n") ? data.slice(0, -1) : data,
                  id,
                };
              }
              event = "";
              data = "";
              id = "";
              continue;
            }
            if (line.startsWith(":")) continue; // comment

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
          }
        }
        // Flush trailing event if stream ended without a final empty line.
        if (data) {
          yield {
            event: event || "message",
            data: data.endsWith("\n") ? data.slice(0, -1) : data,
            id,
          };
        }
      } finally {
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
        try {
          parsed = JSON.parse(event.data);
        } catch {
          continue; // non-JSON SSE line (keepalive, etc.)
        }
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
            model = "claude-sonnet-4-5-20250514",
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
    //   const result = await runToolLoop({
    //     chat: anthropic(key).chat,
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
              input: JSON.parse(tc.function.arguments),
            }));
          }
        }

        if (!toolCalls || toolCalls.length === 0) {
          return { text: textContent, response, iterations: i + 1 };
        }

        // Execute tool calls and append results.
        msgs.push(response.choices ? response.choices[0].message : { role: "assistant", content: response.content });
        for (const call of toolCalls) {
          const result = await onToolCall(call);
          // Anthropic shape
          msgs.push({
            role: "user",
            content: [
              {
                type: "tool_result",
                tool_use_id: call.id,
                content: typeof result === "string" ? result : JSON.stringify(result),
              },
            ],
          });
        }
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
