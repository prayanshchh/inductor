import { CopilotClient } from "@github/copilot-sdk";
import assert from "node:assert/strict";
import readline from "node:readline";
import path from "node:path";

const SOFT_CONTEXT_TOKENS = 248_000;
const HARD_CONTEXT_TOKENS = 250_000;
const DEFAULT_TIMEOUT_MS = 120_000;

const pendingTools = new Map();
let activeSession = null;
let activeClient = null;
let cancelled = false;
let toolCounter = 0;

function write(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

function settlePendingTools(message) {
  for (const resolve of pendingTools.values()) {
    resolve({ output: message, is_error: true });
  }
  pendingTools.clear();
}

async function cancelActive() {
  cancelled = true;
  settlePendingTools("Cancelled by host.");
  try {
    await activeSession?.abort();
  } catch {}
}

const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
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
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    return;
  }
  if (message?.type === "tool_result") {
    const resolve = pendingTools.get(message.id);
    if (resolve) {
      pendingTools.delete(message.id);
      resolve(message);
    }
  } else if (message?.type === "cancel") {
    void cancelActive();
  }
});

function toolResultTimeoutMs() {
  const configured = Number.parseInt(
    process.env.INDUCTOR_COPILOT_TOOL_RESULT_TIMEOUT_MS ?? String(DEFAULT_TIMEOUT_MS),
    10,
  );
  return Number.isFinite(configured) && configured > 0 ? configured : DEFAULT_TIMEOUT_MS;
}

function requestHostTool(definition, args, invocation) {
  const id = invocation?.toolCallId || `copilot-tool-${toolCounter++}`;
  write({ type: "tool_request", id, name: definition.name, input: args ?? {} });
  return new Promise((resolve) => {
    const timeoutId = setTimeout(() => {
      pendingTools.delete(id);
      resolve({
        textResultForLlm: `Inductor timed out waiting for ${definition.name}.`,
        resultType: "failure",
        error: "Host tool result timeout",
      });
    }, toolResultTimeoutMs());
    pendingTools.set(id, (result) => {
      clearTimeout(timeoutId);
      resolve({
        textResultForLlm: String(result.output ?? ""),
        resultType: result.is_error ? "failure" : "success",
        ...(result.is_error ? { error: String(result.output ?? "Tool failed") } : {}),
      });
    });
  });
}

function customTools(definitions) {
  return (Array.isArray(definitions) ? definitions : []).map((definition) => ({
    name: definition.name,
    description: definition.description,
    parameters: definition.input_schema ?? { type: "object", properties: {} },
    handler: (args, invocation) => requestHostTool(definition, args, invocation),
    overridesBuiltInTool: true,
    skipPermission: true,
    defer: "never",
  }));
}

function messageText(message) {
  const pieces = [];
  for (const part of Array.isArray(message?.parts) ? message.parts : []) {
    if (part?.type === "text" && typeof part.text === "string") {
      pieces.push(part.text);
    } else if (part?.type === "image") {
      pieces.push("[Image attached to this turn]");
    }
  }
  return pieces.join("\n");
}

function promptFromRequest(request) {
  const messages = Array.isArray(request.messages) ? request.messages : [];
  if (messages.length === 0) return request.prompt || "Continue.";
  return messages
    .map((message) => `${String(message.role || "message").toUpperCase()}:\n${messageText(message)}`)
    .join("\n\n");
}

function attachmentsFromRequest(request) {
  return (Array.isArray(request.images) ? request.images : []).map((image, index) => ({
    type: "blob",
    data: image.base64_data,
    mimeType: image.mime_type,
    displayName: image.path ? path.basename(image.path) : `image-${index + 1}`,
  }));
}

function compactionThresholds(maxPromptTokens) {
  if (!Number.isFinite(maxPromptTokens) || maxPromptTokens <= HARD_CONTEXT_TOKENS) {
    return {
      backgroundCompactionThreshold: 0.8,
      bufferExhaustionThreshold: 0.95,
    };
  }
  return {
    backgroundCompactionThreshold: SOFT_CONTEXT_TOKENS / maxPromptTokens,
    bufferExhaustionThreshold: HARD_CONTEXT_TOKENS / maxPromptTokens,
  };
}

async function modelPromptLimit(client, modelId) {
  try {
    const models = await client.listModels();
    const model = models.find((candidate) => candidate.id === modelId);
    return model?.capabilities?.limits?.max_prompt_tokens ?? null;
  } catch {
    return null;
  }
}

function checkpointEvent(request, session, latestCompaction) {
  return {
    type: "context_checkpoint",
    provider_id: "copilot",
    model: request.checkpoint_model || request.model,
    kind: "copilot_infinite_session",
    payload: {
      session_id: session.sessionId,
      workspace_path: session.workspacePath ?? null,
      compaction: latestCompaction,
    },
    summary: latestCompaction?.summaryContent ?? request.context_checkpoint?.summary ?? null,
  };
}

async function run(request) {
  const baseDirectory = path.join(request.cwd, ".inductor", "copilot-sdk");
  const client = new CopilotClient({
    mode: "empty",
    workingDirectory: request.cwd,
    baseDirectory,
    gitHubToken: request.github_token,
    useLoggedInUser: false,
    logLevel: "error",
  });
  activeClient = client;
  await client.start();

  const maxPromptTokens = await modelPromptLimit(client, request.model);
  const config = {
    model: request.model,
    workingDirectory: request.cwd,
    gitHubToken: request.github_token,
    systemMessage: {
      mode: "append",
      content: request.system_prompt || undefined,
    },
    tools: customTools(request.tool_definitions),
    availableTools: ["custom:*"],
    excludedTools: ["builtin:*", "mcp:*"],
    infiniteSessions: {
      enabled: true,
      ...compactionThresholds(maxPromptTokens),
    },
  };

  const resumeSessionId = request.context_checkpoint?.payload?.session_id;
  let recoveredSummary = null;
  let session;
  if (typeof resumeSessionId === "string" && resumeSessionId.length > 0) {
    try {
      session = await client.resumeSession(resumeSessionId, config);
    } catch (error) {
      if (typeof request.context_checkpoint?.summary !== "string") throw error;
      recoveredSummary = request.context_checkpoint.summary;
      session = await client.createSession({
        ...config,
        sessionId: `${request.session_id}-recovered-${Date.now()}`,
      });
    }
  } else {
    session = await client.createSession({ ...config, sessionId: request.session_id });
  }
  activeSession = session;

  let latestCompaction = request.context_checkpoint?.payload?.compaction ?? null;
  let sawDelta = false;
  const unsubscribe = session.on((event) => {
    switch (event.type) {
      case "assistant.message_delta":
        if (event.data.deltaContent) {
          sawDelta = true;
          write({ type: "text_delta", text: event.data.deltaContent });
        }
        break;
      case "assistant.message":
        if (!sawDelta && event.data.content) {
          write({ type: "text_delta", text: event.data.content });
        }
        break;
      case "assistant.usage":
        write({
          type: "usage",
          input_tokens: event.data.inputTokens ?? null,
          output_tokens: event.data.outputTokens ?? null,
          cache_read_tokens: event.data.cacheReadTokens ?? null,
          total_cost_usd: event.data.cost ?? null,
        });
        break;
      case "session.compaction_start":
        write({
          type: "compaction",
          phase: "started",
          pre_tokens:
            (event.data.systemTokens ?? 0) +
            (event.data.conversationTokens ?? 0) +
            (event.data.toolDefinitionsTokens ?? 0),
          post_tokens: null,
          details: event.data,
        });
        break;
      case "session.compaction_complete":
        latestCompaction = event.data;
        write({
          type: "compaction",
          phase: event.data.success ? "completed" : "failed",
          pre_tokens: event.data.preCompactionTokens ?? null,
          post_tokens: event.data.postCompactionTokens ?? null,
          summary: event.data.summaryContent ?? null,
          details: event.data,
        });
        write(checkpointEvent(request, session, latestCompaction));
        break;
      case "session.error":
        write({ type: "error", message: event.data.message || "Copilot session failed" });
        break;
      default:
        break;
    }
  });

  try {
    await session.sendAndWait(
      {
        prompt: recoveredSummary
          ? `COPILOT TRAJECTORY SUMMARY FROM THE PREVIOUS CHECKPOINT:\n${recoveredSummary}\n\nNEWER INDUCTOR MESSAGES:\n${promptFromRequest(request)}`
          : promptFromRequest(request),
        attachments: attachmentsFromRequest(request),
      },
      Number.parseInt(process.env.INDUCTOR_COPILOT_IDLE_TIMEOUT_MS ?? "600000", 10),
    );
    if (!cancelled) {
      write(checkpointEvent(request, session, latestCompaction));
      write({ type: "result", stop_reason: "end_turn" });
    }
  } finally {
    unsubscribe();
    settlePendingTools("Copilot session ended.");
    await session.disconnect().catch(() => {});
    await client.stop().catch(() => {});
    activeSession = null;
    activeClient = null;
  }
}

if (process.argv.includes("--self-test")) {
  assert.deepEqual(compactionThresholds(1_000_000), {
    backgroundCompactionThreshold: 0.248,
    bufferExhaustionThreshold: 0.25,
  });
  assert.deepEqual(compactionThresholds(200_000), {
    backgroundCompactionThreshold: 0.8,
    bufferExhaustionThreshold: 0.95,
  });
  const checkpoint = checkpointEvent(
    { model: "gpt-test", context_checkpoint: null },
    { sessionId: "session-1", workspacePath: "/tmp/session-1" },
    { summaryContent: "structured trajectory summary" },
  );
  assert.equal(checkpoint.payload.session_id, "session-1");
  assert.equal(checkpoint.summary, "structured trajectory summary");
  input.close();
} else {
  try {
    const raw = await firstLine;
    if (!raw) process.exit(0);
    await run(JSON.parse(raw));
  } catch (error) {
    if (cancelled) {
      write({ type: "result", stop_reason: "cancelled" });
    } else {
      write({ type: "error", message: error?.stack || String(error) });
    }
    try {
      await activeClient?.forceStop();
    } catch {}
  }
}
