import { createSdkMcpServer, query, tool } from "@anthropic-ai/claude-agent-sdk";
import readline from "node:readline/promises";
import { z } from "zod/v4";

const input = readline.createInterface({
  input: process.stdin,
  crlfDelay: Infinity,
});

let outputClosed = false;
process.stdout.on("error", (error) => {
  if (error?.code === "EPIPE") {
    outputClosed = true;
    cancelActiveQuery();
    return;
  }
  throw error;
});

function write(value) {
  if (outputClosed) return;
  try {
    process.stdout.write(`${JSON.stringify(value)}\n`);
  } catch (error) {
    if (error?.code === "EPIPE") {
      outputClosed = true;
      cancelActiveQuery();
      return;
    }
    throw error;
  }
}

// Tool-permission requests awaiting a decision from the host, keyed by id.
const pendingDecisions = new Map();
const pendingToolResults = new Map();
const inductorToolUseIds = new Set();
let permCounter = 0;
let toolCounter = 0;
let activeAbortController = null;
let cancelled = false;

function cancelPendingDecisions(message = "Cancelled by host.") {
  for (const resolve of pendingDecisions.values()) {
    resolve({ decision: "deny", message });
  }
  pendingDecisions.clear();
}

function cancelPendingToolResults(message = "Cancelled by host.") {
  for (const resolve of pendingToolResults.values()) {
    resolve({ output: message, is_error: true });
  }
  pendingToolResults.clear();
}

function cancelActiveQuery() {
  cancelled = true;
  cancelPendingDecisions();
  cancelPendingToolResults();
  activeAbortController?.abort();
}

// The host streams JSON lines on stdin. The first line is the run request; every
// later line is a `permission_decision` answering a `permission_request`.
let firstLineResolve;
const firstLine = new Promise((resolve) => {
  firstLineResolve = resolve;
});
let sawFirstLine = false;

input.on("line", (line) => {
  if (!sawFirstLine) {
    sawFirstLine = true;
    firstLineResolve(line);
    return;
  }
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg && msg.type === "permission_decision") {
    const pending = pendingDecisions.get(msg.id);
    if (pending) {
      pendingDecisions.delete(msg.id);
      pending(msg);
    }
  } else if (msg && msg.type === "tool_result") {
    const pending = pendingToolResults.get(msg.id);
    if (pending) {
      pendingToolResults.delete(msg.id);
      pending(msg);
    }
  } else if (msg && msg.type === "cancel") {
    cancelActiveQuery();
  }
});

function handleStreamEvent(event) {
  if (event.type === "content_block_delta") {
    const text = event.delta?.text;
    if (typeof text === "string" && text.length > 0) {
      write({ type: "text_delta", text });
    }
  }
}

// Read-only built-in tools auto-run without prompting the user (like Claude
// Code's defaults). Everything else (Write/Edit/Bash/...) goes through the host
// unless the host selected yolo mode.
const READ_ONLY_TOOLS = new Set([
  "Read", "Glob", "Grep", "NotebookRead", "TodoWrite", "WebFetch", "WebSearch",
]);

const HOST_TOOL_RESULT_TIMEOUT_MS = Number.parseInt(
  process.env.INDUCTOR_CLAUDE_TOOL_RESULT_TIMEOUT_MS ?? "120000",
  10
);

function hostToolResultTimeoutMs() {
  return Number.isFinite(HOST_TOOL_RESULT_TIMEOUT_MS) && HOST_TOOL_RESULT_TIMEOUT_MS > 0
    ? HOST_TOOL_RESULT_TIMEOUT_MS
    : 120000;
}

// Permission handler: the model uses the SDK's native tools; we approve reads
// automatically and route mutating tools to the host (Inductor TUI) for a
// grant/deny/allow-for-session decision.
async function requestPermission(toolName, toolInput, opts, approvalPolicy = "on_request") {
  if (toolName === "Skill" || toolName.startsWith("mcp__inductor__")) {
    return { behavior: "allow", updatedInput: toolInput };
  }

  if (approvalPolicy === "never") {
    return { behavior: "allow", updatedInput: toolInput };
  }

  if (READ_ONLY_TOOLS.has(toolName)) {
    return { behavior: "allow", updatedInput: toolInput };
  }

  const id = `perm-${permCounter++}`;
  write({
    type: "permission_request",
    id,
    tool_name: toolName,
    title: opts?.title ?? null,
    description: opts?.description ?? null,
    input: toolInput,
  });

  const decision = await new Promise((resolve) => {
    let settled = false;
    let onAbort;
    const settle = (value) => {
      if (settled) return;
      settled = true;
      pendingDecisions.delete(id);
      if (onAbort && opts?.signal) {
        opts.signal.removeEventListener("abort", onAbort);
      }
      resolve(value);
    };

    onAbort = () => settle({ decision: "deny", message: "Cancelled by host." });
    pendingDecisions.set(id, settle);

    if (opts?.signal?.aborted) {
      onAbort();
    } else if (opts?.signal) {
      opts.signal.addEventListener("abort", onAbort, { once: true });
    }
  });

  if (decision.decision === "deny") {
    return {
      behavior: "deny",
      message:
        typeof decision.message === "string" && decision.message.length > 0
          ? decision.message
          : "The user denied this action.",
    };
  }

  const result = { behavior: "allow", updatedInput: toolInput };
  // "Allow for the rest of the session" — adopt the SDK's own suggestions so it
  // won't prompt again for this tool during the session.
  if (decision.decision === "allow_always" && Array.isArray(opts?.suggestions)) {
    result.updatedPermissions = opts.suggestions;
  }
  return result;
}

function zodForJsonSchema(schema = {}) {
  if (Array.isArray(schema.enum) && schema.enum.every((value) => typeof value === "string")) {
    return schema.enum.length === 0 ? z.string() : z.enum(schema.enum);
  }

  switch (schema.type) {
    case "string":
      return z.string();
    case "integer":
      return z.number().int();
    case "number":
      return z.number();
    case "boolean":
      return z.boolean();
    case "array":
      return z.array(zodForJsonSchema(schema.items ?? {}));
    case "object":
      return z.object(zodShapeFromJsonSchema(schema));
    default:
      return z.any();
  }
}

function zodShapeFromJsonSchema(schema = {}) {
  const required = new Set(Array.isArray(schema.required) ? schema.required : []);
  const shape = {};
  for (const [name, propertySchema] of Object.entries(schema.properties ?? {})) {
    let field = zodForJsonSchema(propertySchema);
    if (!required.has(name)) {
      field = field.optional();
    }
    shape[name] = field;
  }
  return shape;
}

async function requestHostTool(name, args, opts = {}) {
  const id = `tool-${toolCounter++}`;
  write({
    type: "tool_request",
    id,
    name,
    input: args ?? {},
  });

  const result = await new Promise((resolve) => {
    let settled = false;
    let onAbort;
    const timeout = setTimeout(() => {
      settle({
        output: `Timed out waiting for Inductor host tool result for ${name}.`,
        is_error: true,
      });
    }, hostToolResultTimeoutMs());
    const settle = (value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      pendingToolResults.delete(id);
      if (onAbort && opts?.signal) {
        opts.signal.removeEventListener("abort", onAbort);
      }
      resolve(value);
    };

    onAbort = () => settle({ output: "Cancelled by host.", is_error: true });
    pendingToolResults.set(id, settle);

    if (cancelled || opts?.signal?.aborted) {
      onAbort();
    } else if (opts?.signal) {
      opts.signal.addEventListener("abort", onAbort, { once: true });
    }
  });

  return {
    content: [{ type: "text", text: String(result.output ?? "") }],
    isError: result.is_error === true,
  };
}

function createInductorMcpServer(toolDefinitions = []) {
  return createSdkMcpServer({
    name: "inductor",
    version: "1.0.0",
    instructions: "Inductor owns these workspace tools. Tool results come from the host runtime.",
    alwaysLoad: true,
    tools: toolDefinitions.map((definition) =>
      tool(
        definition.name,
        definition.description ?? "Inductor workspace tool.",
        zodShapeFromJsonSchema(definition.input_schema ?? {}),
        (args, extra) => requestHostTool(definition.name, args, extra),
        { alwaysLoad: true }
      )
    ),
  });
}

function toolAliases(toolDefinitions = []) {
  const mcpName = (name) => `mcp__inductor__${name}`;
  const aliases = {
    Read: mcpName("read_file"),
    LS: mcpName("list_dir"),
    Write: mcpName("write_file"),
    Edit: mcpName("edit_file"),
    MultiEdit: mcpName("multi_edit"),
    Grep: mcpName("grep"),
    Glob: mcpName("glob"),
    WebFetch: mcpName("web_fetch"),
    TodoWrite: mcpName("todo_write"),
    Bash: mcpName("bash"),
  };
  for (const definition of toolDefinitions) {
    aliases[definition.name] = mcpName(definition.name);
  }
  return aliases;
}

// Surface tool_use blocks (from assistant messages) and tool_result blocks (from
// the following user message) so the host can render what the agent is doing.
function emitToolBlocks(message) {
  if (message.type === "assistant") {
    for (const block of message.message?.content ?? []) {
      if (block.type === "tool_use") {
        if (typeof block.name === "string" && block.name.startsWith("mcp__inductor__")) {
          inductorToolUseIds.add(block.id);
          continue;
        }
        write({
          type: "tool_use",
          id: block.id,
          name: block.name,
          input: block.input ?? {},
        });
      }
    }
  } else if (message.type === "user") {
    for (const block of message.message?.content ?? []) {
      if (block.type === "tool_result") {
        if (inductorToolUseIds.delete(block.tool_use_id)) {
          continue;
        }
        let text = "";
        if (typeof block.content === "string") {
          text = block.content;
        } else if (Array.isArray(block.content)) {
          text = block.content
            .map((c) => (typeof c?.text === "string" ? c.text : ""))
            .join("");
        }
        write({
          type: "tool_result",
          id: block.tool_use_id,
          output: text,
          is_error: block.is_error === true,
        });
      }
    }
  }
}

function claudeContentBlocks(parts = []) {
  const content = [];
  for (const part of parts) {
    if (part?.type === "text" && typeof part.text === "string" && part.text.length > 0) {
      content.push({ type: "text", text: part.text });
    } else if (part?.type === "image" && part.image) {
      content.push({
        type: "image",
        source: {
          type: "base64",
          media_type: part.image.mime_type,
          data: part.image.base64_data,
        },
      });
    }
  }
  return content;
}

function createFallbackPrompt(prompt = "", images = []) {
  if (!Array.isArray(images) || images.length === 0) {
    return prompt;
  }

  const parts = [];
  if (typeof prompt === "string" && prompt.trim().length > 0) {
    parts.push({ type: "text", text: prompt });
  }
  for (const image of images) {
    parts.push({ type: "image", image });
  }

  return (async function* () {
    yield {
      type: "user",
      message: {
        role: "user",
        content: claudeContentBlocks(parts),
      },
      parent_tool_use_id: null,
    };
  })();
}

function createPromptFromMessages(messages = [], fallbackPrompt = "", fallbackImages = []) {
  if (!Array.isArray(messages) || messages.length === 0) {
    return createFallbackPrompt(fallbackPrompt, fallbackImages);
  }

  return (async function* () {
    for (const message of messages) {
      const role = message?.role === "assistant" ? "assistant" : "user";
      const content = claudeContentBlocks(message?.parts ?? []);
      if (content.length === 0) continue;
      yield {
        type: role,
        message: {
          role,
          content,
        },
        parent_tool_use_id: null,
      };
    }
  })();
}

async function run(request) {
  const abortController = new AbortController();
  activeAbortController = abortController;
  if (cancelled) {
    abortController.abort();
  }

  const promptContent = createPromptFromMessages(request.messages, request.prompt, request.images);

  const toolDefinitions = Array.isArray(request.tool_definitions) ? request.tool_definitions : [];
  const inductorMcp = createInductorMcpServer(toolDefinitions);
  const approvalPolicy =
    typeof request.approval_policy === "string" ? request.approval_policy : "never";

  // Claude still gets the Claude Code agent prompt, but local workspace tools
  // are Inductor MCP tools. Rust owns execution, permission gating, patches,
  // and tool result rendering.
  const options = {
    cwd: request.cwd,
    systemPrompt: {
      type: "preset",
      preset: "claude_code",
      append: typeof request.system_prompt === "string" ? request.system_prompt : undefined,
    },
    includePartialMessages: true,
    permissionMode: approvalPolicy === "never" ? "bypassPermissions" : "default",
    allowDangerouslySkipPermissions: approvalPolicy === "never",
    tools: [],
    mcpServers: { inductor: inductorMcp },
    toolAliases: toolAliases(toolDefinitions),
    // Load the user-level setting source so the Claude Code subscription
    // login/credentials are available for the turn. Without it the SDK has no
    // usable auth and the API rejects the request with `401 Invalid
    // authentication credentials`, even though `claude` works in the terminal.
    // Project/local sources stay excluded so per-project settings don't
    // override Inductor's own tools, permissions, and system prompt.
    settingSources: ["user"],
    canUseTool: (toolName, toolInput, opts) =>
      requestPermission(toolName, toolInput, opts, approvalPolicy),
    abortController,
  };

  if (request.model) {
    options.model = request.model;
  }

  try {
    const stream = query({
      prompt: promptContent,
      options,
    });

    for await (const message of stream) {
      emitToolBlocks(message);
      switch (message.type) {
        case "stream_event":
          handleStreamEvent(message.event);
          break;
        case "result":
          // Emit real token usage + cost reported by the SDK.
          if (message.usage || typeof message.total_cost_usd === "number") {
            const u = message.usage || {};
            write({
              type: "usage",
              input_tokens: u.input_tokens ?? null,
              output_tokens: u.output_tokens ?? null,
              cache_read_tokens: u.cache_read_input_tokens ?? null,
              total_cost_usd:
                typeof message.total_cost_usd === "number"
                  ? message.total_cost_usd
                  : null,
            });
          }
          if (message.subtype === "error" || message.is_error) {
            write({
              type: "error",
              message: message.result || "Claude Agent SDK request failed",
            });
          } else {
            write({
              type: "result",
              stop_reason: message.stop_reason || "end_turn",
            });
          }
          return;
        default:
          break;
      }
    }
  } catch (error) {
    if (cancelled || abortController.signal.aborted) {
      write({ type: "result", stop_reason: "cancelled" });
      return;
    }
    throw error;
  } finally {
    if (activeAbortController === abortController) {
      activeAbortController = null;
    }
  }

  write({ type: "result", stop_reason: "end_turn" });
}

// Verify the SDK has a usable login by running a minimal probe query.
// We never read the secret ourselves; we let the SDK use its own login and
// only observe whether it can reach a non-error result.
async function checkAuth(request) {
  try {
    const stream = query({
      prompt: "ping",
      options: {
        cwd: request.cwd,
        systemPrompt: { type: "preset", preset: "claude_code" },
        permissionMode: "dontAsk",
        settingSources: ["user", "project", "local"],
        maxTurns: 1,
      },
    });

    for await (const message of stream) {
      if (message.type === "result") {
        if (message.subtype === "error" || message.is_error) {
          write({
            type: "auth_check",
            ok: false,
            error: message.result || "Claude Agent SDK auth check failed",
          });
        } else {
          write({ type: "auth_check", ok: true });
        }
        return;
      }
    }

    // Stream ended without an explicit result but also without throwing.
    write({ type: "auth_check", ok: true });
  } catch (error) {
    write({
      type: "auth_check",
      ok: false,
      error: error instanceof Error ? error.message : String(error),
    });
  }
}

try {
  const line = await firstLine;
  if (line === undefined) {
    throw new Error("missing bridge request");
  }

  const request = JSON.parse(line);
  if (request.mode === "check_auth") {
    await checkAuth(request);
  } else {
    await run(request);
  }
} catch (error) {
  write({
    type: "error",
    message: error instanceof Error ? error.message : String(error),
  });
  process.exitCode = 1;
} finally {
  input.close();
}
