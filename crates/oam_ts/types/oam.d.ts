// oam's own TypeScript declarations: the `oam:` module family (js/mcp.js,
// js/test_runner.js, js/ai.js, js/permissions.js) and the `oam` global
// (crates/oam_engine/src/ops.rs + the serve() half in js/bootstrap.js).
//
// oam_ts injects this file into every check -- as an extra root file for a
// bare-file target, through a generated wrapper tsconfig for a project (see
// crates/oam_ts/src/decls.rs) -- so `import { McpServer } from "oam:mcp"`
// type-checks with no tsconfig edit and no types package to install.
//
// This is a SCRIPT, not a module: no top-level import/export. A top-level
// export would turn every `declare module` below into an augmentation of a
// module TypeScript cannot resolve, and the ambient declarations would stop
// applying.
//
// The web types used below -- ReadableStream, Headers, Response,
// AbortSignal -- are the ones oam's own runtime installs, and they are taken
// from the checker's lib rather than redeclared here: a private structural
// copy would not be assignable from the real thing a user already holds. A
// project that narrows `lib` to exclude them (and installs no @types/node,
// which republishes them) will see the error inside THIS file, which is the
// honest place for it -- the project has told TypeScript its runtime has no
// fetch, and oam's does.
//
// PARITY IS GATED. `cargo run -p xtask -- conformance` diffs the value names
// declared here against the module objects the runtime actually publishes
// and fails both ways: a JS export with no declaration here (the program
// type-errors on a name that works at runtime), and a name declared here
// that the JS does not export (a declaration that promises an API nobody
// implemented). Deliberate omissions live in conformance/oam-module-types.json
// with a reason. Signatures are NOT machine-checked -- only names are -- so
// read the JS before you write one.

// -------------------------------------------------------------- oam:mcp --

declare module "oam:mcp" {
  /** JSON Schema for a tool's arguments, passed through to the client verbatim. */
  interface McpJsonSchema {
    type?: string;
    properties?: Record<string, unknown>;
    required?: string[];
    [key: string]: unknown;
  }

  interface McpContentBlock {
    type: string;
    text?: string;
    [key: string]: unknown;
  }

  /** What a tool handler may return. A string or a plain value is wrapped in
   *  a single text block by the server; an object that already carries
   *  `content` is passed through unchanged. */
  interface McpToolResult {
    content: McpContentBlock[];
    isError?: boolean;
    [key: string]: unknown;
  }

  interface McpToolConfig<Args = Record<string, unknown>> {
    description?: string;
    /** `parameters` is the documented spelling; `inputSchema` is accepted as
     *  an alias. Neither is required -- an omitted schema is published as an
     *  empty object schema. */
    parameters?: McpJsonSchema;
    inputSchema?: McpJsonSchema;
    handler: (args: Args) => McpToolResult | string | unknown | Promise<McpToolResult | string | unknown>;
  }

  /** A resource handler's return value. `Uint8Array` and `ArrayBuffer` are
   *  base64-encoded into a blob; anything else that is not a string and does
   *  not already carry `contents` is JSON-stringified into a text block. */
  type McpResourceResult = string | Uint8Array | ArrayBuffer | { contents: unknown[] } | unknown;

  interface McpResourceConfig {
    name?: string;
    description?: string;
    /** Defaults to "text/plain". */
    mimeType?: string;
    handler: (uri: string) => McpResourceResult | Promise<McpResourceResult>;
  }

  interface McpResourceTemplateConfig {
    name?: string;
    description?: string;
    mimeType?: string;
    /** `params` carries the `{name}` placeholders matched out of the URI. */
    handler: (
      uri: string,
      params: Record<string, string>,
    ) => McpResourceResult | Promise<McpResourceResult>;
  }

  interface McpPromptArgument {
    name: string;
    description?: string;
    required?: boolean;
    [key: string]: unknown;
  }

  interface McpPromptMessage {
    role: string;
    content: McpContentBlock;
  }

  interface McpPromptConfig<Args = Record<string, unknown>> {
    description?: string;
    arguments?: McpPromptArgument[];
    handler: (
      args: Args,
    ) =>
      | string
      | McpPromptMessage[]
      | { messages: McpPromptMessage[] }
      | unknown
      | Promise<string | McpPromptMessage[] | { messages: McpPromptMessage[] } | unknown>;
  }

  interface McpServeOptions {
    /** Omitted: stdio when stdin is not a TTY, else http. */
    transport?: "stdio" | "http";
    /** http only. Defaults to $PORT, then 3000. */
    port?: number;
    /** http only. Defaults to "127.0.0.1". */
    host?: string;
  }

  /** What `serve()` resolves to on the http transport. `server` is the
   *  node:http Server; it is `unknown` because oam ships no node:http
   *  declarations of its own and a reference to @types/node here would make
   *  every check fail on a project that does not install them. */
  interface McpHttpServer {
    server: unknown;
    port: number;
    host: string;
  }

  class McpServer {
    constructor(options: { name: string; version?: string });
    tool<Args = Record<string, unknown>>(name: string, config: McpToolConfig<Args>): this;
    resource(uri: string, config: McpResourceConfig): this;
    resourceTemplate(uriTemplate: string, config: McpResourceTemplateConfig): this;
    prompt<Args = Record<string, unknown>>(name: string, config: McpPromptConfig<Args>): this;
    /** stdio only: called when the client closes the input stream. */
    onClose(fn: () => void): this;
    /** Resolves with the bound server on the http transport, and with
     *  `undefined` on stdio -- where the promise stays pending for the life
     *  of the session and settles when the client disconnects. */
    serve(options?: McpServeOptions): Promise<McpHttpServer | undefined>;
  }

  /** The MCP protocol revision this server answers `initialize` with. */
  const PROTOCOL_VERSION: string;

  /** Every `oam:` module also publishes its module object as the default
   *  export, so `import mcp from "oam:mcp"` works. Named imports are the
   *  documented spelling; this is declared because the export is real, and a
   *  declaration that omitted it would type-error code that runs. */
  const moduleObject: {
    McpServer: typeof McpServer;
    PROTOCOL_VERSION: typeof PROTOCOL_VERSION;
  };

  export { McpServer, PROTOCOL_VERSION };
  export default moduleObject;
}

// ------------------------------------------------------------- oam:test --

declare module "oam:test" {
  interface Matchers {
    not: Matchers;
    /** Awaits the value under test, then applies the matcher to what it
     *  resolved with; rejects the assertion if the promise threw. */
    readonly resolves: AsyncMatchers;
    /** The mirror of `resolves`: the matcher sees the rejection reason. */
    readonly rejects: AsyncMatchers;
    toBe(expected: unknown): void;
    toEqual(expected: unknown): void;
    toStrictEqual(expected: unknown): void;
    toBeTruthy(): void;
    toBeFalsy(): void;
    toBeNull(): void;
    toBeUndefined(): void;
    toBeDefined(): void;
    toBeNaN(): void;
    toBeGreaterThan(n: number | bigint): void;
    toBeGreaterThanOrEqual(n: number | bigint): void;
    toBeLessThan(n: number | bigint): void;
    toBeLessThanOrEqual(n: number | bigint): void;
    /** Passes when the difference is below 10 ** -digits / 2 (digits: 2). */
    toBeCloseTo(n: number, digits?: number): void;
    toBeInstanceOf(ctor: new (...args: any[]) => unknown): void;
    toContain(item: unknown): void;
    toContainEqual(item: unknown): void;
    toHaveLength(length: number): void;
    /** `path` is dot-separated. With no `value`, only presence is checked. */
    toHaveProperty(path: string, value?: unknown): void;
    /** A RegExp is tested; anything else is a substring match. */
    toMatch(pattern: string | RegExp): void;
    toMatchObject(expected: object): void;
    /** Requires the value under test to be a function; it is called here. */
    toThrow(expected?: string | RegExp | (new (...args: any[]) => unknown)): void;
    toHaveBeenCalled(): void;
    toHaveBeenCalledTimes(n: number): void;
    toHaveBeenCalledWith(...args: unknown[]): void;
    toHaveBeenLastCalledWith(...args: unknown[]): void;
  }

  /** The same matcher set behind `.resolves` / `.rejects`, awaited. */
  interface AsyncMatchers {
    not: AsyncMatchers;
    toBe(expected: unknown): Promise<void>;
    toEqual(expected: unknown): Promise<void>;
    toStrictEqual(expected: unknown): Promise<void>;
    toBeTruthy(): Promise<void>;
    toBeFalsy(): Promise<void>;
    toBeNull(): Promise<void>;
    toBeUndefined(): Promise<void>;
    toBeDefined(): Promise<void>;
    toBeNaN(): Promise<void>;
    toBeGreaterThan(n: number | bigint): Promise<void>;
    toBeGreaterThanOrEqual(n: number | bigint): Promise<void>;
    toBeLessThan(n: number | bigint): Promise<void>;
    toBeLessThanOrEqual(n: number | bigint): Promise<void>;
    toBeCloseTo(n: number, digits?: number): Promise<void>;
    toBeInstanceOf(ctor: new (...args: any[]) => unknown): Promise<void>;
    toContain(item: unknown): Promise<void>;
    toContainEqual(item: unknown): Promise<void>;
    toHaveLength(length: number): Promise<void>;
    toHaveProperty(path: string, value?: unknown): Promise<void>;
    toMatch(pattern: string | RegExp): Promise<void>;
    toMatchObject(expected: object): Promise<void>;
    toThrow(expected?: string | RegExp | (new (...args: any[]) => unknown)): Promise<void>;
    toHaveBeenCalled(): Promise<void>;
    toHaveBeenCalledTimes(n: number): Promise<void>;
    toHaveBeenCalledWith(...args: unknown[]): Promise<void>;
    toHaveBeenLastCalledWith(...args: unknown[]): Promise<void>;
  }

  type TestBody = () => void | Promise<void>;
  type HookBody = () => void | Promise<void>;

  /** A bare number is the timeout in ms (default 5000). */
  type TestOptions = number | { timeout?: number };

  interface TestFn {
    (name: string, fn: TestBody, options?: TestOptions): void;
    skip(name: string, fn: TestBody, options?: TestOptions): void;
    /** Restricts the run to `.only` tests and suites across the whole file. */
    only(name: string, fn: TestBody, options?: TestOptions): void;
    /** Reported as todo; the body is never run, so none is taken. */
    todo(name: string): void;
  }

  interface DescribeFn {
    (name: string, fn: () => void): void;
    skip(name: string, fn: () => void): void;
    only(name: string, fn: () => void): void;
  }

  interface ExpectFn {
    (actual: unknown): Matchers;
    /** Fails the current test outright. */
    fail(message?: string): never;
  }

  interface MockResult {
    type: "return" | "throw";
    value: unknown;
  }

  interface MockFn<Args extends any[] = any[], Return = any> {
    (...args: Args): Return;
    mock: { calls: Args[]; results: MockResult[]; contexts: unknown[] };
    mockImplementation(fn: (...args: Args) => Return): MockFn<Args, Return>;
    /** Consumed by the next call only; queued in order. */
    mockImplementationOnce(fn: (...args: Args) => Return): MockFn<Args, Return>;
    mockReturnValue(value: Return): MockFn<Args, Return>;
    mockReturnValueOnce(value: Return): MockFn<Args, Return>;
    mockResolvedValue(value: unknown): MockFn<Args, Return>;
    mockResolvedValueOnce(value: unknown): MockFn<Args, Return>;
    mockRejectedValue(value: unknown): MockFn<Args, Return>;
    mockRejectedValueOnce(value: unknown): MockFn<Args, Return>;
    /** Drops recorded calls/results/contexts; keeps the implementation. */
    mockClear(): MockFn<Args, Return>;
    /** mockClear plus: queued and default implementations go back to the
     *  one the mock was created with. */
    mockReset(): MockFn<Args, Return>;
  }

  interface SpyFn<Args extends any[] = any[], Return = any> extends MockFn<Args, Return> {
    /** Puts the original property back. Also done for every spy after each
     *  test, so a leaked spy cannot reach the next one. */
    mockRestore(): void;
  }

  interface FakeTimers {
    /** Replaces the global timer functions and Date.now until `restore()`. */
    enable(options?: { now?: number }): void;
    /** Advances the clock by `ms`, firing due callbacks in due order --
     *  including ones scheduled by the callbacks it just fired. */
    tick(ms: number): void;
    /** Drains every pending timer. Intervals are dropped rather than
     *  rescheduled (Node test-runner parity), so this cannot spin forever. */
    runAll(): void;
    setSystemTime(time: number | Date): void;
    /** The fake clock while enabled, the real Date.now otherwise. */
    now(): number;
    pendingCount(): number;
    restore(): void;
  }

  interface Mock {
    fn<Args extends any[] = any[], Return = any>(
      impl?: (...args: Args) => Return,
    ): MockFn<Args, Return>;
    /** Replaces `object[key]` with a spy that still calls through. */
    spyOn<Obj extends object, Key extends keyof Obj>(object: Obj, key: Key): SpyFn;
    timers: FakeTimers;
    /** Clears recorded calls on every mock created this run. */
    clearAll(): void;
    /** Restores every active spy. */
    restoreAll(): void;
  }

  const describe: DescribeFn;
  const test: TestFn;
  /** The same function object as `test`. */
  const it: TestFn;
  const expect: ExpectFn;
  const beforeAll: (fn: HookBody) => void;
  const afterAll: (fn: HookBody) => void;
  const beforeEach: (fn: HookBody) => void;
  const afterEach: (fn: HookBody) => void;
  const mock: Mock;

  /** The module object (see oam:mcp for why this is declared). It also
   *  carries `__run`, the entry point the Rust test runner calls after the
   *  file evaluates; that one is deliberately undeclared -- writing a test
   *  never means calling it -- and is recorded as such in
   *  conformance/oam-module-types.json. */
  const moduleObject: {
    describe: DescribeFn;
    test: TestFn;
    it: TestFn;
    expect: ExpectFn;
    beforeAll: (fn: HookBody) => void;
    afterAll: (fn: HookBody) => void;
    beforeEach: (fn: HookBody) => void;
    afterEach: (fn: HookBody) => void;
    mock: Mock;
  };

  export {
    describe,
    test,
    it,
    expect,
    beforeAll,
    afterAll,
    beforeEach,
    afterEach,
    mock,
  };
  export default moduleObject;
}

// --------------------------------------------------------------- oam:ai --

declare module "oam:ai" {
  interface SSEEvent {
    /** The `event:` field, or "message" when the frame carried none. */
    event: string;
    data: string;
    id: string;
  }

  /** Parse a byte stream of server-sent events. Comment lines are skipped,
   *  multi-line `data:` fields are joined, and `retry:` is ignored (this API
   *  never reconnects). */
  function parseSSEStream(readableStream: ReadableStream<Uint8Array>): AsyncGenerator<SSEEvent>;

  interface StreamChatOptions {
    url: string;
    headers?: Record<string, string>;
    /** A string is sent as-is; anything else is JSON-stringified. */
    body: unknown;
    signal?: AbortSignal;
    /** Pull the text delta out of one parsed SSE frame. Returning null or
     *  undefined skips the frame. The default handles the OpenAI
     *  (`choices[0].delta.content`) and Anthropic (`delta.text`) shapes. */
    extractDelta?: (parsed: any, event: SSEEvent) => string | null | undefined;
  }

  /** POST JSON, stream SSE back, yield content deltas. Throws on a non-2xx
   *  response and stops at the `[DONE]` sentinel. */
  function streamChat(options: StreamChatOptions): AsyncGenerator<string>;

  interface ChatMessage {
    role: string;
    content: unknown;
  }

  interface ProviderChatOptions {
    model?: string;
    signal?: AbortSignal;
    [key: string]: unknown;
  }

  /** A provider preset: `chat` streams deltas over that provider's URL and
   *  auth-header shape. */
  interface Provider {
    chat(messages: ChatMessage[], options?: ProviderChatOptions): AsyncGenerator<string>;
  }

  /** baseURL defaults to https://api.openai.com/v1 . */
  function openai(apiKey: string, baseURL?: string): Provider;

  /** baseURL defaults to https://api.anthropic.com/v1 . `system` and
   *  `max_tokens` (4096) are passed through `options`. */
  function anthropic(apiKey: string, baseURL?: string): Provider;

  interface ToolCall {
    id: string;
    name: string;
    input: unknown;
  }

  interface RunToolLoopOptions {
    /** Called as `chat(messages, options)` and AWAITED as a whole response
     *  (`stream: false` is forced), so the streaming `chat` returned by
     *  openai() / anthropic() is not a drop-in here -- pass a non-streaming
     *  call of your own. Anthropic (`content` blocks) and OpenAI
     *  (`choices[0].message`) response shapes are both understood. */
    chat: (messages: ChatMessage[], options: Record<string, unknown>) => unknown | Promise<unknown>;
    tools?: unknown[];
    messages: ChatMessage[];
    /** Runs one tool call and returns its result; a non-string is
     *  JSON-stringified into the tool_result block. */
    onToolCall: (call: ToolCall) => unknown | Promise<unknown>;
    onDelta?: (text: string) => void;
    /** Default 10. Hitting it returns `exhausted: true` rather than throwing. */
    maxIterations?: number;
    chatOptions?: Record<string, unknown>;
  }

  interface ToolLoopResult {
    text: string;
    response: unknown;
    iterations: number;
    exhausted?: boolean;
  }

  /** Run the call-tool-respond cycle until the model answers without asking
   *  for a tool, or `maxIterations` is spent. */
  function runToolLoop(options: RunToolLoopOptions): Promise<ToolLoopResult>;

  /** The default `extractDelta`: OpenAI's `choices[0].delta.content`, else
   *  Anthropic's `delta.text`, else null. */
  function defaultExtractDelta(parsed: any): string | null;

  /** The module object (see oam:mcp for why this is declared). */
  const moduleObject: {
    parseSSEStream: typeof parseSSEStream;
    streamChat: typeof streamChat;
    openai: typeof openai;
    anthropic: typeof anthropic;
    runToolLoop: typeof runToolLoop;
    defaultExtractDelta: typeof defaultExtractDelta;
  };

  export {
    parseSSEStream,
    streamChat,
    openai,
    anthropic,
    runToolLoop,
    defaultExtractDelta,
  };
  export default moduleObject;
}

// ------------------------------------------------------ oam:permissions --

declare module "oam:permissions" {
  interface PermissionDescriptor {
    /** The runtime answers "read", "write", "net", "env", "child" and "ffi";
     *  every other name is reported denied rather than rejected. */
    name: string;
    /** Matched against the --allow-read / --allow-write path list. */
    path?: string;
    /** Matched against the --allow-net host list. */
    url?: string;
  }

  /** oam has no interactive prompt, so the state is decided by the flags the
   *  process started with: there is no "prompt" state to observe. */
  interface PermissionStatus {
    state: "granted" | "denied";
    onchange: null;
    toString(): string;
  }

  interface Permissions {
    query(descriptor: PermissionDescriptor): Promise<PermissionStatus>;
    /** No dynamic prompting: this is `query` under the Deno-shaped name. */
    request(descriptor: PermissionDescriptor): Promise<PermissionStatus>;
    /** Always resolves "denied" -- the runtime cannot drop a grant mid-run
     *  yet, so this reports the state it would leave you in, not a change
     *  it made. */
    revoke(descriptor: PermissionDescriptor): Promise<PermissionStatus>;
  }

  const permissions: Permissions;

  /** The module object (see oam:mcp for why this is declared). */
  const moduleObject: { permissions: Permissions };

  export { permissions };
  export default moduleObject;
}

// -------------------------------------------------------- the oam global --

/** The request handed to an `oam.serve` handler. It is NOT a web `Request`:
 *  the body is read from the runtime on demand and only these members
 *  exist. */
interface OamServeRequest {
  method: string;
  url: string;
  headers: Headers;
  readonly bodyUsed: boolean;
  arrayBuffer(): Promise<ArrayBuffer>;
  bytes(): Promise<Uint8Array>;
  text(): Promise<string>;
  json(): Promise<any>;
}

type OamServeHandler = (request: OamServeRequest) => Response | Promise<Response>;

interface OamServeOptions {
  fetch: OamServeHandler;
  /** Defaults to 0 -- the OS picks a free port, reported back as `port`. */
  port?: number;
  /** Defaults to "127.0.0.1". */
  hostname?: string;
}

interface OamServer {
  port: number;
  hostname: string;
  close(): void;
}

interface OamGlobal {
  /** The running oam's version, e.g. "0.14.0". */
  readonly version: string;
  /** Resolve after `ms` milliseconds. Tokio-backed, and it keeps the process
   *  alive the way a pending timer does. */
  sleep(ms: number): Promise<void>;
  /** Read a file as UTF-8. Rejects with an Error on any I/O failure. */
  readTextFile(path: string): Promise<string>;
  /** Serve HTTP with a web-standard handler. Requests are dispatched
   *  concurrently -- the accept loop never awaits a handler -- and a
   *  ReadableStream body streams to the client as it is produced. */
  serve(options: OamServeOptions | OamServeHandler): Promise<OamServer>;
}

declare const oam: OamGlobal;
