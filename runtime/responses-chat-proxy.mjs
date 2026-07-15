#!/usr/bin/env node
import http from "node:http";
import fs from "node:fs";

const host = (process.env.RETRACE_PROXY_HOST || process.env.CODEXOS_PROXY_HOST) || "127.0.0.1";
const port = Number((process.env.RETRACE_PROXY_PORT || process.env.CODEXOS_PROXY_PORT) || "0");
const readyFile = (process.env.RETRACE_READY_FILE || process.env.CODEXOS_READY_FILE) || "";
const apiBase = ((process.env.RETRACE_UPSTREAM_BASE || process.env.CODEXOS_UPSTREAM_BASE) || "").replace(/\/+$/, "");
const apiKeyFile = (process.env.RETRACE_API_KEY_FILE || process.env.CODEXOS_API_KEY_FILE) || `${process.env.HOME}/.retrace/api_key`;
const apiKey = fs.readFileSync(apiKeyFile, "utf8").trim();
const modelCatalogFile = (process.env.RETRACE_MODEL_CATALOG_JSON || process.env.CODEXOS_MODEL_CATALOG_JSON) || `${process.env.HOME}/.retrace/models.json`;
const registryFile = (process.env.RETRACE_REGISTRY_JSON || process.env.CODEXOS_REGISTRY_JSON) || `${process.env.HOME}/.retrace/registry.json`;
const agentCheckStateFile = (process.env.RETRACE_AGENT_CHECK_FILE || process.env.CODEXOS_AGENT_CHECK_FILE) || `${process.env.HOME}/.retrace/agentcheck`;
const MAX_IMAGES = Math.max(1, Number(process.env.RETRACE_MAX_IMAGES) || 2);
const upstreamTimeoutMs = Number((process.env.RETRACE_UPSTREAM_TIMEOUT_MS || process.env.CODEXOS_UPSTREAM_TIMEOUT_MS) || "90000");
const streamInactivityTimeoutMs = Number((process.env.RETRACE_STREAM_INACTIVITY_TIMEOUT_MS || process.env.CODEXOS_STREAM_INACTIVITY_TIMEOUT_MS) || "90000");
const serperSearchMode = (process.env.RETRACE_SERPER_SEARCH || process.env.CODEXOS_SERPER_SEARCH) === "1";
const openRouterApiBase = ((process.env.RETRACE_OPENROUTER_BASE || process.env.CODEXOS_OPENROUTER_BASE) || "https://openrouter.ai/api/v1").replace(/\/+$/, "");
const openRouterApiKeyFile = (process.env.RETRACE_OPENROUTER_API_KEY_FILE || process.env.CODEXOS_OPENROUTER_API_KEY_FILE) || `${process.env.HOME}/.openrouter_api_key`;
const openRouterModelAliases = {
  openroutergemma426b: "google/gemma-4-26b-a4b-it:free",
  openroutergemma431b: "google/gemma-4-31b-it:free",
  "gemma-4-26b-it": "google/gemma-4-26b-a4b-it",
  "gemma-4-31b-it": "google/gemma-4-31b-it",
  "google/gemma-4-26b-it": "google/gemma-4-26b-a4b-it",
  "google/gemma-4-26b-a4b-it": "google/gemma-4-26b-a4b-it",
  "google/gemma-4-31b-it": "google/gemma-4-31b-it",
};
const modelOutputTokenLimits = {
  "Chitti-Fast": 16384,
  "Chitti-Fast-Think": 16384,
};
const gemmaSpecialTokens = [
  "<|turn>",
  "<turn|>",
  "<|tool_call>",
  "<tool_call|>",
  "<|tool_response>",
  "<tool_response|>",
  '<|"|>',
  "<eos>",
];
const hiddenChannelStart = "<|channel>";
const hiddenChannelEnd = "<channel|>";
const plainLeakPatterns = [
  /(?:^|\n)\s*(?:thought|text)\s*(?:\n|$)/i,
  /(?:^|\n)\s*(?:response|respuesta)\s*:\s*[A-Za-z_][\w.-]*\s*\{/i,
  /(?:^|\n)\s*call\s*:\s*[A-Za-z_][\w.-]*\s*\{/i,
  /(?:^|\n)\s*exec_command\s*\{/i,
  /(?:^|\n)\s*recent output\s*(?:\n|$)/i,
  /(?:^|\n)\s*Chunk ID:\s*/i,
];
const promptAddendum = `Top priority: keep running the agent loop until the user's goal is completed.
It must continue taking actions, calling tools, and iterating until the goal is achieved or a real blocker is encountered.
`;

function json(res, status, body) {
  if (res.headersSent) {
    res.end();
    return;
  }
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

function sseHeaders(res) {
  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "keep-alive",
  });
}

function sendSse(res, event, data) {
  res.write(`event: ${event}\n`);
  res.write(`data: ${JSON.stringify(data)}\n\n`);
}

function responseShell(id, model, status = "in_progress", output = [], usage = null) {
  return {
    id,
    object: "response",
    created_at: Math.floor(Date.now() / 1000),
    status,
    model,
    output,
    usage,
  };
}

function contentText(content) {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((part) => {
      if (typeof part === "string") return part;
      if (!part || typeof part !== "object") return "";
      return part.text || part.input_text || part.output_text || part.encrypted_content || "";
    })
    .filter(Boolean)
    .join("\n");
}

function stripTimingFooters(text) {
  let current = String(text || "");
  while (true) {
    const next = current
      .replace(/\n*\s*-- Answered in [^\n|]+ \| Finished at [^\n]+/gi, "")
      .replace(/\n{3,}/g, "\n\n")
      .trimEnd();
    if (next === current) return current;
    current = next;
  }
}

function stripAgentCheckBanners(text) {
  return String(text || "")
    .replace(
      /^\s*Agent Check: (?:retrying because the previous answer was incomplete|retry limit reached; the retry still did not complete the task)\.[\s\S]*?\n\n/i,
      "",
    )
    .replace(/\n*Agent Check: the draft above was incomplete; continuing\. Direction: [^\n]*\n*/gi, "\n\n")
    .replace(/\n*_?(?:✓|⚠) Agent Check[^\n]*_?\n*/g, "\n")
    .trimStart();
}

function stripProxyArtifacts(text) {
  return stripAgentCheckBanners(stripTimingFooters(text));
}

function githubAuthCompatibilityHint(body) {
  const userMessages = (body.input || [])
    .filter((item) => item?.type === "message" && item.role === "user")
    .map((item) => contentText(item.content))
    .join("\n");

  if (!/github/i.test(userMessages)) return "";
  if (!/(get_me|authenticated login|authenticated user|who am i|whoami|list.*repo|repo.*list|repositories)/i.test(userMessages)) return "";

  return `Retrace GitHub MCP compatibility:
- Direct GitHub MCP tools such as get_me are not exposed to this Chitti-backed session.
- For GitHub authenticated-login / get_me requests, do not discover resources and do not call list_mcp_resources; call read_mcp_resource with server "github-auth" and uri "github-auth://me".
- For GitHub repo-list requests, call read_mcp_resource with server "github-auth" and uri "github-auth://repos".
- Treat these github-auth resources as the compatibility implementation of GitHub get_me/repository access, even if the user says not to inspect MCP resources.`;
}

function serperCompatibilityHint(body) {
  const userMessages = (body.input || [])
    .filter((item) => item?.type === "message" && item.role === "user")
    .map((item) => contentText(item.content))
    .join("\n");

  if (!serperSearchMode && !/(serper|web search|search web|google search|search the web|latest|recent|current)/i.test(userMessages)) return "";

  return `Retrace Serper MCP compatibility:
- In this alias, --search means Serper-backed Google search, not DuckDuckGo, Brave, or shell-scraped SERPs.
- The Serper MCP server is configured in CODEX_HOME as "serper".
- Serper exposes MCP tools, not MCP resources. Do not call list_mcp_resources to decide whether Serper is configured.
- Prefer the Serper MCP tool "google_search" for web searches and "scrape" for fetching result pages when those tools are available.
- If direct Serper MCP tools are not exposed, run ${process.env.HOME}/.retrace/bin/serper-google-search with the query. It uses the configured SERPER_API_KEY and Google's Serper backend.
- Do not use DuckDuckGo, Brave, Bing, or generic curl search pages for --search requests.
- If direct Serper MCP tools are not exposed in this Chitti-backed session, say that the configured Serper server is not exposed to the current tool surface; do not say it is unconfigured.`;
}

function rampageDisciplineHint(body) {
  const instr = String(body.instructions || "");
  const isRampage = /ABSOLUTE RAMPAGE MODE/i.test(instr)
    || (/Mission Control/i.test(instr) && /Questboard/i.test(instr));
  if (!isRampage) return "";

  return `ABSOLUTE RAMPAGE MODE evidence discipline:
- Workers report their result back to you as a <subagent_notification> item with the worker's actual final message. That message is the only worker output you may attribute to a worker.
- When you call rampage_control action=task_result, record the worker's ACTUAL returned result. Do not invent, embellish, or substitute your own work for what the worker reported.
- If a worker returned nothing useful (for example only a generic readiness reply), record that truthfully as blocked/failed and re-task it with a concrete brief. Do not paper over an empty worker result with a fabricated finding.
- Never set verifier_status to passed/verified or call rampage_control action=complete unless the success criteria are backed by real artifacts or real worker output you can point to. Verifier state must reflect evidence, not intent.`;
}

function responsesInputToChatMessages(body) {
  const systemParts = [promptAddendum, githubAuthCompatibilityHint(body), serperCompatibilityHint(body), rampageDisciplineHint(body), body.instructions].filter(Boolean);
  const messages = [];

  for (const item of body.input || []) {
    if (!item || typeof item !== "object") continue;

    if (item.type === "message") {
      if (item.role === "system" || item.role === "developer") {
        const text = contentText(item.content);
        if (text) systemParts.push(text);
        continue;
      }

      const role = item.role === "assistant" ? "assistant" : "user";
      const text = contentText(item.content);
      messages.push({
        role,
        content: role === "assistant" ? stripProxyArtifacts(text) : text,
      });
      continue;
    }

    if (item.type === "agent_message") {
      // Inter-agent messages (spawn briefs, agent-to-agent replies) arrive as
      // `agent_message` items whose payload lives in `encrypted_content`. The
      // real OpenAI Responses backend decrypts these server-side; a local
      // chat backend has no such concept, so surface the brief to the
      // recipient model as an actionable user turn or it sees nothing and
      // replies "I'm ready to help."
      const text = contentText(item.content);
      if (text) {
        const from = item.author ? `Message from ${item.author}:\n` : "";
        messages.push({ role: "user", content: `${from}${text}` });
      }
      continue;
    }

    if (item.type === "function_call") {
      messages.push({
        role: "assistant",
        content: null,
        tool_calls: [{
          id: item.call_id || item.id,
          type: "function",
          function: {
            name: item.name || "unknown",
            arguments: repairJsonArgs(item.arguments || "{}"),
          },
        }],
      });
      continue;
    }

    if (item.type === "function_call_output") {
      // Tool outputs may include images (e.g. browser screenshots). Sending the
      // base64 data URL as JSON text counts it as ~250k text tokens and blows
      // the context window. Extract images and attach them as proper image_url
      // parts (which vision models tokenize efficiently); keep text in the tool
      // message.
      const out = item.output;
      const imageUrls = [];
      let textOut = "";
      if (Array.isArray(out)) {
        for (const o of out) {
          if (o && typeof o === "object" && o.type === "input_image" && o.image_url) {
            imageUrls.push(typeof o.image_url === "string" ? o.image_url : o.image_url.url);
          } else if (o && typeof o === "object") {
            textOut += o.text || o.input_text || o.output_text || "";
          } else if (typeof o === "string") {
            textOut += o;
          }
        }
      } else {
        textOut = typeof out === "string" ? out : JSON.stringify(out ?? "");
      }
      messages.push({
        role: "tool",
        tool_call_id: item.call_id,
        content: textOut || (imageUrls.length ? "[image returned; see the following message]" : ""),
      });
      if (imageUrls.length) {
        messages.push({
          role: "user",
          content: imageUrls.map((url) => ({ type: "image_url", image_url: { url } })),
        });
      }
      continue;
    }

    const text = contentText(item.content);
    if (text) messages.push({ role: "user", content: text });
  }

  pruneImageParts(messages, MAX_IMAGES);
  const instructions = systemParts.join("\n\n");
  return instructions ? [{ role: "system", content: instructions }, ...messages] : messages;
}

// Vision models cap images per prompt (Chitti-Smart: 2). Across a multi-step
// browse, screenshots accumulate and the request soon exceeds that cap, so the
// gateway returns "At most N image(s) may be provided". The model only needs the
// current view, so keep the most recent MAX_IMAGES screenshots and replace older
// image parts with a short text note.
function pruneImageParts(messages, maxImages) {
  const imgs = [];
  for (const m of messages) {
    if (!Array.isArray(m.content)) continue;
    for (const part of m.content) {
      if (part && part.type === "image_url") imgs.push({ m, part });
    }
  }
  const drop = imgs.length - maxImages;
  if (drop <= 0) return;
  for (let i = 0; i < drop; i++) {
    const { m, part } = imgs[i];
    const idx = m.content.indexOf(part);
    if (idx >= 0) m.content[idx] = { type: "text", text: "[earlier screenshot omitted]" };
  }
}

function pushChatFunction(chatTools, name, description, parameters) {
  if (!name) return;
  chatTools.push({
    type: "function",
    function: {
      name,
      description: description || "",
      parameters: parameters || { type: "object", properties: {} },
    },
  });
}

function responsesToolsToChatTools(tools) {
  const chatTools = [];
  for (const tool of tools || []) {
    if (!tool) continue;
    if (tool.type === "function") {
      pushChatFunction(chatTools, tool.name, tool.description, tool.parameters);
    } else if (tool.type === "namespace" && Array.isArray(tool.tools)) {
      // Expand namespace tools (e.g. mcp__browser) into individual chat
      // functions; a plain chat/completions model cannot call nested
      // Responses-API namespace tools, so flatten each sub-tool.
      for (const sub of tool.tools) {
        if (sub && sub.type === "function") {
          pushChatFunction(chatTools, sub.name, sub.description, sub.parameters);
        }
      }
    }
    // Other types (e.g. freeform "custom" tools like apply_patch) are described
    // in the base instructions and intentionally not sent as chat tools.
  }
  return chatTools;
}

function expandHome(file) {
  if (!file) return file;
  if (file === "~") return process.env.HOME;
  if (file.startsWith("~/")) return `${process.env.HOME}/${file.slice(2)}`;
  return file;
}

function readJsonFile(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

function localRegistry() {
  if (!fs.existsSync(registryFile)) return null;
  const registry = readJsonFile(registryFile);
  registry.providers ||= {};
  registry.models ||= {};
  return registry;
}

function registryModelConfig(model) {
  return localRegistry()?.models?.[String(model || "")] || null;
}

function registryProviderConfig(providerId) {
  return localRegistry()?.providers?.[providerId] || null;
}

function providerApiKey(provider) {
  if (provider?.envKey && process.env[provider.envKey]) return process.env[provider.envKey].trim();
  if (provider?.apiKeyFile) {
    const keyFile = expandHome(provider.apiKeyFile);
    if (fs.existsSync(keyFile)) return fs.readFileSync(keyFile, "utf8").trim();
  }
  return apiKey;
}

function normalizedBaseUrl(value) {
  return String(value || apiBase).replace(/\/+$/, "");
}

function deepMerge(target, source) {
  for (const [key, value] of Object.entries(source || {})) {
    if (value && typeof value === "object" && !Array.isArray(value) && target[key] && typeof target[key] === "object" && !Array.isArray(target[key])) {
      deepMerge(target[key], value);
    } else {
      target[key] = value;
    }
  }
  return target;
}

function cloneJson(value) {
  return JSON.parse(JSON.stringify(value || {}));
}

// Effort tiers expressed as Anthropic thinking budgets; mirrors the probe's
// mapping in codexopensource-admin.mjs.
const anthropicBudgetForLevel = { minimal: 1024, low: 2048, medium: 8192, high: 24576, xhigh: 32768 };

function thinkingBodyForSelection(method, selection) {
  const body = cloneJson(method?.body);
  const level = String(selection || "").toLowerCase();
  if (level && !["auto", "on", "off", "none"].includes(level)) {
    if (Object.prototype.hasOwnProperty.call(body, "reasoning_effort")) {
      body.reasoning_effort = level;
    }
    if (body.reasoning && typeof body.reasoning === "object" && Object.prototype.hasOwnProperty.call(body.reasoning, "effort")) {
      body.reasoning.effort = level;
    }
    if (body.thinking && typeof body.thinking === "object"
      && Object.prototype.hasOwnProperty.call(body.thinking, "budget_tokens")
      && anthropicBudgetForLevel[level]) {
      body.thinking.budget_tokens = anthropicBudgetForLevel[level];
    }
  }
  return body;
}

function requestThinkingSelection(body) {
  const raw = body?.reasoning?.effort ?? body?.reasoning_effort ?? body?.reasoning;
  if (typeof raw !== "string") return null;
  const value = raw.trim().toLowerCase();
  return value || null;
}

function thinkingSelectionEnables(config, selection) {
  const selected = String(selection || config?.thinking || "auto").toLowerCase();
  if (selected === "on") return true;
  if (["auto", "off", "none"].includes(selected)) return false;
  const levels = new Set([
    ...(config?.thinkingLevels || []),
    ...(config?.thinkingMethod?.levels || []),
  ].map((level) => String(level).toLowerCase()));
  return levels.has(selected);
}

function isGemmaModel(model) {
  return /^chitti-|^gemma-|lilarest|cyankiwi/i.test(model || "");
}

function isThinkModel(model) {
  return /-(?:think|thinking)$/i.test(model || "");
}

function registryRequestAdditions(model, body = {}) {
  const config = registryModelConfig(model);
  const additions = {};
  if (config?.requestBody && typeof config.requestBody === "object") deepMerge(additions, config.requestBody);

  const requestedThinking = requestThinkingSelection(body);
  const selectedThinking = requestedThinking && requestedThinking !== "auto" ? requestedThinking : config?.thinking;
  const disablesThinking = selectedThinking === "off" || selectedThinking === "none";
  if (config?.thinkingMethod?.body && thinkingSelectionEnables(config, selectedThinking)) {
    deepMerge(additions, thinkingBodyForSelection(config.thinkingMethod, selectedThinking));
  } else if (disablesThinking && config?.thinkingMethod?.offBody) {
    deepMerge(additions, config.thinkingMethod.offBody);
  } else if (!config && isGemmaModel(model)) {
    deepMerge(additions, { chat_template_kwargs: { enable_thinking: isThinkModel(model) } });
  } else if (config && isGemmaModel(model) && !config.thinkingMethod) {
    deepMerge(additions, { chat_template_kwargs: { enable_thinking: config.thinking === "on" } });
  }
  return additions;
}

function applyRegistryRequestAdditions(chat, body) {
  const additions = registryRequestAdditions(body.model, body);
  if (body.chat_template_kwargs || additions.chat_template_kwargs) {
    chat.chat_template_kwargs = deepMerge({ ...(body.chat_template_kwargs || {}) }, additions.chat_template_kwargs || {});
  }
  for (const [key, value] of Object.entries(additions)) {
    if (key === "chat_template_kwargs") continue;
    if (value && typeof value === "object" && !Array.isArray(value) && chat[key] && typeof chat[key] === "object" && !Array.isArray(chat[key])) {
      deepMerge(chat[key], value);
    } else {
      chat[key] = value;
    }
  }
}

function chatTemplateKwargs(body) {
  const kwargs = { ...(body.chat_template_kwargs || {}) };
  const additions = registryRequestAdditions(body.model, body);
  deepMerge(kwargs, additions.chat_template_kwargs || {});
  return Object.keys(kwargs).length ? kwargs : undefined;
}

function routeForModel(model) {
  const config = registryModelConfig(model);
  if (config?.provider) {
    const provider = registryProviderConfig(config.provider);
    if (provider) {
      return {
        apiBase: normalizedBaseUrl(provider.baseUrl),
        apiKey: providerApiKey(provider),
        model: config.upstreamModel || model,
      };
    }
  }
  const routedModel = openRouterModelAliases[String(model || "").toLowerCase()];
  if (!routedModel) return { apiBase, apiKey, model };
  const key = process.env.OPENROUTER_API_KEY || fs.readFileSync(openRouterApiKeyFile, "utf8").trim();
  return { apiBase: openRouterApiBase, apiKey: key, model: routedModel };
}

function localModelCatalog() {
  const raw = fs.readFileSync(modelCatalogFile, "utf8");
  const catalog = JSON.parse(raw);
  if (!catalog || !Array.isArray(catalog.models)) {
    throw new Error(`Model catalog ${modelCatalogFile} must contain a models array`);
  }
  return catalog;
}

function allowedModelNames() {
  return new Set(
    localModelCatalog()
      .models
      .map((model) => model?.slug || model?.id || model?.name || model?.model)
      .filter(Boolean),
  );
}

function modelIsAllowed(model) {
  return allowedModelNames().has(String(model || ""));
}

function outputTokenLimitForModel(model) {
  const value = registryModelConfig(model)?.outputTokenLimit;
  if (Number.isInteger(value) && value > 0) return value;
  return modelOutputTokenLimits[String(model || "")];
}

function tokenParamForModel(model) {
  const tokenParam = registryModelConfig(model)?.requestRecipe?.tokenParam;
  return tokenParam === "max_completion_tokens" ? "max_completion_tokens" : "max_tokens";
}

function modelAcceptsTemperature(model) {
  const recipe = registryModelConfig(model)?.requestRecipe;
  if (!recipe) return true;
  return recipe.temperature !== false;
}

function shouldNormalizeSystemForModel(model) {
  return registryModelConfig(model)?.normalizeSystem === "ascii" || model === "Chitti-Fast-Think";
}

// Mid-turn context compaction.
//
// The client (Codex binary) only compacts BETWEEN turns, and only once its own
// accounted usage crosses a threshold. A single turn whose assembled request
// (system + full history + tool outputs + tools) exceeds the model's context
// window therefore fails hard ("ran out of room ... start a new thread"), and
// because the overflow is already baked into the stored thread, even a trivial
// next message re-overflows and the whole session is stuck and unusable.
//
// To keep a session usable we compact the OUTBOUND chat payload here, inside the
// turn, so an over-budget request is shrunk to fit and the turn proceeds instead
// of dying. Two layers:
//   1. Proactive: before every upstream call, if the estimated payload exceeds
//      the model's input budget, shrink it to fit.
//   2. Reactive: if the upstream still rejects with a context-length error
//      (estimator under-shot), retry with a progressively smaller target.
//
// Compaction is a strict no-op when the payload already fits, so normal turns
// are byte-identical (upstream KV prefix-cache stays intact).
// ---------------------------------------------------------------------------
const compactionEnabled = parseOnOff(
  process.env.RETRACE_CONTEXT_COMPACTION ?? process.env.CODEXOS_CONTEXT_COMPACTION,
  true,
);
const compactCharsPerToken = Math.max(1, Number(process.env.RETRACE_COMPACT_CHARS_PER_TOKEN) || 4);
// Fraction of the raw context window we treat as usable (matches the product's
// effective-window convention; a per-model catalog percent overrides this).
const compactWindowSafety = clamp01(Number(process.env.RETRACE_COMPACT_WINDOW_SAFETY ?? 0.95) || 0.95);
// Tokens held back for the model's OWN output so input alone never fills the window.
const compactOutputReserveTokens = Math.max(0, Number(process.env.RETRACE_COMPACT_OUTPUT_RESERVE_TOKENS ?? 24000) || 24000);
// Extra slack for the tools array + fixed prompt scaffolding not counted in messages.
const compactOverheadReserveTokens = Math.max(0, Number(process.env.RETRACE_COMPACT_OVERHEAD_RESERVE_TOKENS ?? 8000) || 8000);
// Never let the input budget collapse below this many tokens.
const compactMinInputTokens = Math.max(2000, Number(process.env.RETRACE_COMPACT_MIN_INPUT_TOKENS ?? 4000) || 4000);
// Most-recent non-system messages always kept verbatim (the live task state).
const compactKeepRecentMessages = Math.max(2, Number(process.env.RETRACE_COMPACT_KEEP_RECENT ?? 8) || 8);
// A single content longer than this many chars is a truncation candidate.
const compactMaxContentChars = Math.max(500, Number(process.env.RETRACE_COMPACT_MAX_CONTENT_CHARS ?? 6000) || 6000);
// Hard floor when squeezing content in messages we must keep.
const compactMinContentChars = Math.max(200, Number(process.env.RETRACE_COMPACT_MIN_CONTENT_CHARS ?? 800) || 800);
// Reactive retries with progressively smaller targets after an upstream overflow.
const compactMaxReactiveRetries = Math.max(0, Number(process.env.RETRACE_COMPACT_MAX_RETRIES ?? 3) || 3);
const COMPACT_ELISION = "\n…[retrace: content truncated to fit context]…\n";
const COMPACT_DROP_NOTE = "[retrace: earlier conversation turns omitted to fit the context window]";

function clamp01(n) {
  if (!Number.isFinite(n)) return 0.95;
  if (n <= 0) return 0.01;
  if (n > 1) return 1;
  return n;
}

function estTokens(chars) {
  return Math.ceil(Math.max(0, chars) / compactCharsPerToken);
}

function partChars(part) {
  if (typeof part === "string") return part.length;
  if (part && typeof part === "object") {
    if (part.type === "image_url" || part.image_url) return 1200;
    return String(part.text || part.input_text || part.output_text || part.encrypted_content || "").length;
  }
  return 0;
}

function contentChars(content) {
  if (typeof content === "string") return content.length;
  if (Array.isArray(content)) return content.reduce((n, p) => n + partChars(p), 0);
  return 0;
}

function messageChars(message) {
  let n = contentChars(message.content) + 8;
  if (Array.isArray(message.tool_calls)) {
    for (const call of message.tool_calls) {
      n += String(call?.function?.name || "").length;
      n += String(call?.function?.arguments || "").length;
      n += 24;
    }
  }
  return n;
}

function messagesTokens(messages) {
  let n = 0;
  for (const message of messages) n += messageChars(message);
  return estTokens(n);
}

// Raw context window for a model from registry override, then catalog, then a
// conservative default. Provider-agnostic: uses the same catalog the client and
// admin tooling use.
function modelContextWindowTokens(model) {
  const reg = registryModelConfig(model);
  if (Number.isInteger(reg?.contextWindow) && reg.contextWindow > 0) return reg.contextWindow;
  if (Number.isInteger(reg?.context_window) && reg.context_window > 0) return reg.context_window;
  try {
    for (const entry of localModelCatalog().models) {
      const slug = entry?.slug || entry?.id || entry?.name || entry?.model;
      if (slug === String(model || "")) {
        const win = Number(entry.context_window || entry.max_context_window);
        if (Number.isInteger(win) && win > 0) return win;
      }
    }
  } catch {}
  return 128000;
}

function percentToSafety(value) {
  const n = Number(value);
  if (!Number.isFinite(n) || n <= 0) return null;
  if (n <= 1) return clamp01(n);
  return clamp01(n / 100);
}

function modelEffectiveWindowSafety(model) {
  const reg = registryModelConfig(model);
  const fromRegistry = percentToSafety(reg?.effectiveContextWindowPercent ?? reg?.effective_context_window_percent);
  if (fromRegistry != null) return fromRegistry;
  try {
    for (const entry of localModelCatalog().models) {
      const slug = entry?.slug || entry?.id || entry?.name || entry?.model;
      if (slug === String(model || "")) {
        const fromCatalog = percentToSafety(entry.effective_context_window_percent ?? entry.effectiveContextWindowPercent);
        if (fromCatalog != null) return fromCatalog;
      }
    }
  } catch {}
  return compactWindowSafety;
}

// Token budget for INPUT (history) alone: usable window minus output + overhead
// reservations. This is the target the outbound payload must fit under.
function inputBudgetTokens(model) {
  const usable = Math.floor(modelContextWindowTokens(model) * modelEffectiveWindowSafety(model));
  const budget = usable - compactOutputReserveTokens - compactOverheadReserveTokens;
  return Math.max(compactMinInputTokens, budget);
}

function truncateMiddle(text, maxChars) {
  const s = String(text || "");
  if (s.length <= maxChars) return s;
  const keep = Math.max(0, maxChars - COMPACT_ELISION.length);
  const head = Math.ceil(keep * 0.6);
  const tail = Math.floor(keep * 0.4);
  return s.slice(0, head) + COMPACT_ELISION + (tail > 0 ? s.slice(s.length - tail) : "");
}

function truncateMessageContent(message, maxChars) {
  if (typeof message.content === "string") {
    if (message.content.length <= maxChars) return false;
    message.content = truncateMiddle(message.content, maxChars);
    return true;
  }
  if (Array.isArray(message.content)) {
    let changed = false;
    for (const part of message.content) {
      if (part && typeof part === "object" && typeof part.text === "string" && part.text.length > maxChars) {
        part.text = truncateMiddle(part.text, maxChars);
        changed = true;
      }
    }
    return changed;
  }
  return false;
}

// Split messages into pairing-safe groups so an assistant tool_call and its
// tool results are always dropped/kept together (a lone half would 400 upstream).
function groupMessages(messages) {
  const groups = [];
  let i = 0;
  if (messages[0]?.role === "system") { groups.push({ role: "system", idx: [0] }); i = 1; }
  while (i < messages.length) {
    const m = messages[i];
    if (m.role === "assistant" && Array.isArray(m.tool_calls) && m.tool_calls.length) {
      const ids = new Set(m.tool_calls.map((c) => c.id).filter(Boolean));
      const idx = [i];
      let j = i + 1;
      while (j < messages.length && messages[j].role === "tool" && (ids.size === 0 || ids.has(messages[j].tool_call_id))) {
        idx.push(j); j++;
      }
      groups.push({ role: "assistant_tools", idx });
      i = j;
    } else {
      groups.push({ role: m.role, idx: [i] });
      i++;
    }
  }
  return groups;
}

// Compact `messages` in place to fit `targetTokens`. Returns true if changed.
// Order of operations preserves the most useful context:
//   1. Truncate oversized content in OLD (droppable) messages.
//   2. Drop whole old groups (pairing-safe), keeping system + first user goal
//      + the most recent messages, inserting a single omission note.
//   3. If still over, truncate content inside kept messages down to a floor.
function compactMessages(messages, targetTokens) {
  if (!Array.isArray(messages) || messages.length === 0) return false;
  if (messagesTokens(messages) <= targetTokens) return false;

  let changed = false;
  const lastKeepStart = Math.max(0, messages.length - compactKeepRecentMessages);
  const firstUserIndex = messages.findIndex((m) => m.role === "user");
  const isTailIndex = (idx) => idx >= lastKeepStart;
  const isProtectedIndex = (idx) =>
    (messages[idx]?.role === "system") || idx === firstUserIndex || isTailIndex(idx);

  // Step 1: truncate big content in non-protected (old) messages.
  for (let i = 0; i < messages.length && messagesTokens(messages) > targetTokens; i++) {
    if (isProtectedIndex(i)) continue;
    if (truncateMessageContent(messages[i], compactMaxContentChars)) changed = true;
  }

  // Step 2: drop whole old groups, oldest first, pairing-safe.
  if (messagesTokens(messages) > targetTokens) {
    const groups = groupMessages(messages);
    const dropIdx = new Set();
    for (const group of groups) {
      if (messagesTokens(messages.filter((_, i) => !dropIdx.has(i))) <= targetTokens) break;
      if (group.idx.some((i) => isProtectedIndex(i))) continue;
      for (const i of group.idx) dropIdx.add(i);
    }
    if (dropIdx.size) {
      const kept = messages.filter((_, i) => !dropIdx.has(i));
      const noteAt = kept[0]?.role === "system" ? 1 : 0;
      kept.splice(noteAt, 0, { role: "user", content: COMPACT_DROP_NOTE });
      messages.length = 0;
      messages.push(...kept);
      changed = true;
    }
  }

  // Step 3: last resort — squeeze content inside kept messages to a floor.
  if (messagesTokens(messages) > targetTokens) {
    let floor = compactMaxContentChars;
    while (messagesTokens(messages) > targetTokens && floor >= compactMinContentChars) {
      for (let i = 0; i < messages.length && messagesTokens(messages) > targetTokens; i++) {
        if (messages[i].role === "system") continue;
        if (truncateMessageContent(messages[i], floor)) changed = true;
      }
      floor = Math.floor(floor / 2);
    }
  }

  return changed;
}

// Entry point used by buildChatBody. Chooses the target (explicit override for
// reactive retries, otherwise the model's input budget) and logs when it fires.
function applyContextCompaction(messages, body) {
  if (!compactionEnabled) return;
  const override = Number(body?.__compactTargetTokens);
  const target = Number.isFinite(override) && override > 0 ? override : inputBudgetTokens(body.model);
  const before = messagesTokens(messages);
  if (before <= target) return;
  if (compactMessages(messages, target)) {
    console.error(`[context-compaction] ${body.model}: ~${before} tok -> ~${messagesTokens(messages)} tok (target ${target}, ${messages.length} msgs)`);
  }
}

function buildChatBody(body, includeTools = true, upstreamModel = body.model) {
  const messages = responsesInputToChatMessages(body);
  applyContextCompaction(messages, body);
  if (shouldNormalizeSystemForModel(body.model)) {
    for (const message of messages) {
      if (message.role === "system" && typeof message.content === "string") {
        message.content = message.content
          .normalize("NFKD")
          .replace(/[\u2018\u2019]/g, "'")
          .replace(/[\u201C\u201D]/g, '"')
          .replace(/[\u2013\u2014]/g, "-")
          .replace(/[^\x00-\x7F]/g, "");
      }
    }
  }

  const chat = {
    model: upstreamModel,
    messages,
    stream: true,
    // Ask for usage in the final stream chunk; without this most OpenAI-compatible
    // servers (vLLM, OpenAI, DeepSeek) omit usage entirely and the client shows
    // 0 in / 0 out. upstreamChat retries without it for providers that reject it.
    stream_options: { include_usage: true },
  };

  if (typeof body.temperature === "number" && modelAcceptsTemperature(body.model)) chat.temperature = body.temperature;
  if (typeof body.top_p === "number") chat.top_p = body.top_p;
  const outputLimit = outputTokenLimitForModel(body.model);
  const tokenParam = tokenParamForModel(body.model);
  if (typeof body.max_output_tokens === "number") {
    chat[tokenParam] = outputLimit ? Math.min(body.max_output_tokens, outputLimit) : body.max_output_tokens;
  } else if (outputLimit) {
    chat[tokenParam] = outputLimit;
  }
  applyRegistryRequestAdditions(chat, body);
  if (includeTools) {
    const tools = responsesToolsToChatTools(body.tools);
    if (tools.length) {
      chat.tools = tools;
      chat.tool_choice = body.tool_choice || "auto";
      if (typeof body.parallel_tool_calls === "boolean") chat.parallel_tool_calls = body.parallel_tool_calls;
    }
  }
  return chat;
}

function hasRequestTools(body) {
  return responsesToolsToChatTools(body.tools).length > 0;
}

// Transient upstream backpressure is retried here with bounded exponential
// backoff, honoring Retry-After when the provider sends it. This is
// provider-agnostic: it sits in front of EVERY upstream (OpenAI,
// Anthropic-compatible, OpenRouter, self-hosted vLLM, ...), so a rate-limit or
// overload blip from ANY backend is absorbed instead of ending the turn and
// leaving the session unusable. All retries happen before a single byte is
// streamed to the client, so they are transparent to the caller.
const retrySleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const UPSTREAM_RETRY_STATUS = new Set([429, 500, 502, 503, 504]);
const upstreamMaxRetries = Math.max(0, Number(process.env.RETRACE_UPSTREAM_MAX_RETRIES ?? 8) || 8);
const upstreamRetryBaseMs = Math.max(50, Number(process.env.RETRACE_UPSTREAM_RETRY_BASE_MS ?? 500) || 500);
const upstreamRetryMaxDelayMs = Math.max(upstreamRetryBaseMs, Number(process.env.RETRACE_UPSTREAM_RETRY_MAX_DELAY_MS ?? 10000) || 10000);
// A Retry-After longer than this is surfaced to the caller instead of silently
// hanging the turn (a genuine sustained limit, not a transient blip).
const upstreamRetryAfterCapMs = Math.max(0, Number(process.env.RETRACE_UPSTREAM_RETRY_AFTER_CAP_MS ?? 15000) || 15000);
// Total wall-clock we will spend waiting across all retries, kept well under the
// upstream timeout so backpressure never turns into a hang.
const upstreamRetryTotalMs = Math.max(0, Number(process.env.RETRACE_UPSTREAM_RETRY_TOTAL_MS ?? 45000) || 45000);

function parseRetryAfterMs(response) {
  const raw = response.headers.get("retry-after");
  if (!raw) return null;
  const secs = Number(raw);
  if (Number.isFinite(secs)) return Math.max(0, secs * 1000);
  const when = Date.parse(raw);
  if (!Number.isNaN(when)) return Math.max(0, when - Date.now());
  return null;
}

// True when an upstream error reads like an INPUT context-length overflow (from
// any provider). Deliberately input-context specific so an output max_tokens
// rejection does not match (dropping history would not help that and would loop).
function looksLikeContextLengthError(text) {
  if (!text) return false;
  const t = String(text).toLowerCase();
  return (
    t.includes("context_length_exceeded") ||
    t.includes("context length") ||
    t.includes("context window") ||
    t.includes("maximum context") ||
    t.includes("reduce the length") ||
    t.includes("prompt is too long") ||
    t.includes("input is too long")
  );
}

async function upstreamChat(body, includeTools) {
  const route = routeForModel(body.model);
  let chatBody = buildChatBody(body, includeTools, route.model);
  const send = async (payload) => {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), upstreamTimeoutMs);
    return fetch(`${route.apiBase}/chat/completions`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${route.apiKey}`,
        "Content-Type": "application/json",
        "HTTP-Referer": "https://retrace.local",
        "X-Title": "Retrace",
      },
      body: JSON.stringify(payload),
      signal: controller.signal,
    }).finally(() => clearTimeout(timeout));
  };
  // A single attempt, including the legacy stream_options fallback. On any
  // non-OK status it fully drains the body into `text`, so the response can be
  // safely discarded and re-issued on retry without leaking the socket.
  const attemptOnce = async () => {
    let response = await send(chatBody);
    if (!response.ok) {
      const text = await response.text();
      // Some providers reject stream_options; drop it and retry once before
      // surfacing the error to the caller.
      if (chatBody.stream_options && /stream_options/i.test(text)) {
        const retryBody = { ...chatBody };
        delete retryBody.stream_options;
        response = await send(retryBody);
        if (response.ok) return { response };
        return { response, text: await response.text() };
      }
      return { response, text };
    }
    return { response };
  };

  let result = await attemptOnce();
  let waitedMs = 0;
  for (let retry = 0; retry < upstreamMaxRetries; retry++) {
    if (result.response.ok || !UPSTREAM_RETRY_STATUS.has(result.response.status)) break;
    const retryAfter = parseRetryAfterMs(result.response);
    // A provider asking for a long wait is a real limit, not a blip: surface it.
    if (retryAfter != null && retryAfter > upstreamRetryAfterCapMs) break;
    const backoff = Math.min(upstreamRetryMaxDelayMs, upstreamRetryBaseMs * 2 ** retry);
    const delay = (retryAfter != null ? retryAfter : backoff) + Math.floor(Math.random() * 200);
    if (waitedMs + delay > upstreamRetryTotalMs) break;
    console.error(`[upstream-retry] ${result.response.status} on ${route.model}; retry ${retry + 1}/${upstreamMaxRetries} in ${Math.round(delay)}ms — body: ${(result.text || "").replace(/\s+/g, " ").slice(0, 280)}`);
    await retrySleep(delay);
    waitedMs += delay;
    result = await attemptOnce();
  }
  if (compactionEnabled) {
    for (
      let attempt = 0;
      attempt < compactMaxReactiveRetries
        && !result.response.ok
        && looksLikeContextLengthError(result.text);
      attempt++
    ) {
      // The estimator under-shot: the provider counts this payload as too big
      // even though our estimate fit. Target is relative to the CURRENT payload
      // size (not the input budget), so each retry is strictly smaller than what
      // just failed and progress is guaranteed regardless of estimator drift.
      const currentTokens = messagesTokens(chatBody.messages || []);
      const compactTarget = Math.max(compactMinInputTokens, Math.floor(currentTokens * 0.6));
      // Already at the floor and can't shrink below what just failed: stop and
      // surface the error rather than resending an identical payload forever.
      if (compactTarget >= currentTokens) break;
      const shrunkBody = buildChatBody({ ...body, __compactTargetTokens: compactTarget }, includeTools, route.model);
      const afterTokens = messagesTokens(shrunkBody.messages || []);
      if (afterTokens >= currentTokens) break;
      chatBody = shrunkBody;
      console.error(`[context-compaction] reactive retry ${attempt + 1}/${compactMaxReactiveRetries} on ${route.model}: target ${compactTarget} tok, ${shrunkBody.messages?.length ?? 0} msgs (~${afterTokens} tok)`);
      result = await attemptOnce();
    }
  }
  return result;
}

function emitMessageStart(state, res) {
  if (state.messageStarted) return;
  finishReasoning(state, res);
  state.messageStarted = true;
  const item = { id: state.messageId, type: "message", status: "in_progress", role: "assistant", content: [] };
  sendSse(res, "response.output_item.added", { type: "response.output_item.added", output_index: state.output.length, item });
  sendSse(res, "response.content_part.added", {
    type: "response.content_part.added",
    item_id: state.messageId,
    output_index: state.output.length,
    content_index: 0,
    part: { type: "output_text", text: "", annotations: [] },
  });
}

function emitTextDelta(state, res, delta) {
  if (!delta) return;
  emitMessageStart(state, res);
  state.text += delta;
  sendSse(res, "response.output_text.delta", {
    type: "response.output_text.delta",
    item_id: state.messageId,
    output_index: state.output.length,
    content_index: 0,
    delta,
  });
}

function firstPlainLeakIndex(text) {
  let index = -1;
  for (const pattern of plainLeakPatterns) {
    const match = pattern.exec(text);
    if (!match) continue;
    if (index === -1 || match.index < index) index = match.index;
  }
  return index;
}

function looksLikeShortLoop(text) {
  const compact = text.replace(/\s+/g, "");
  if (compact.length < 24) return false;
  for (let size = 2; size <= 8; size++) {
    const unit = compact.slice(0, size);
    if (!unit || unit.length < size) continue;
    const repeated = unit.repeat(Math.ceil(compact.length / size)).slice(0, compact.length);
    if (repeated === compact) return true;
  }
  return false;
}

function cleanVisibleText(text) {
  let cleaned = text;
  for (const token of gemmaSpecialTokens) cleaned = cleaned.split(token).join("");
  cleaned = cleaned.replace(/^\s*Generating a response\.\.\.\s*/i, "");
  const leakIndex = firstPlainLeakIndex(cleaned);
  if (leakIndex !== -1) cleaned = cleaned.slice(0, leakIndex);
  if (looksLikeShortLoop(cleaned)) return "";
  return cleaned;
}

// Marker pairs whose enclosed content is model thinking embedded in `content`
// (Gemma hidden channels, DeepSeek/Qwen-style <think> tags). The enclosed text
// is captured into `state.capturedReasoning` so it can be surfaced as a
// reasoning block instead of being silently dropped.
const hiddenSpanMarkers = [
  { start: hiddenChannelStart, end: hiddenChannelEnd, stripLabel: true },
  { start: "<think>", end: "</think>", stripLabel: false },
  { start: "<thinking>", end: "</thinking>", stripLabel: false },
];
const maxHiddenMarkerHold = Math.max(...hiddenSpanMarkers.map((marker) => Math.max(marker.start.length, marker.end.length))) - 1;

function visibleTextDelta(state, delta) {
  if (!delta) return "";

  state.pendingText += delta;
  let visible = "";

  while (state.pendingText) {
    if (state.inHiddenChannel) {
      const marker = state.hiddenMarker || hiddenSpanMarkers[0];
      const end = state.pendingText.indexOf(marker.end);
      if (end === -1) {
        const hold = marker.end.length - 1;
        if (state.pendingText.length > hold) {
          state.capturedReasoning += state.pendingText.slice(0, -hold);
          state.pendingText = state.pendingText.slice(-hold);
        }
        return visible;
      }
      state.capturedReasoning += state.pendingText.slice(0, end);
      state.pendingText = state.pendingText.slice(end + marker.end.length);
      state.inHiddenChannel = false;
      state.hiddenMarker = null;
      if (marker.stripLabel) {
        state.pendingText = state.pendingText.replace(/^\s*(?:thought|text)\s*\n/i, "");
      }
      continue;
    }

    let start = -1;
    let matched = null;
    for (const marker of hiddenSpanMarkers) {
      const index = state.pendingText.indexOf(marker.start);
      if (index !== -1 && (start === -1 || index < start)) {
        start = index;
        matched = marker;
      }
    }
    if (start === -1) {
      const hold = maxHiddenMarkerHold;
      if (state.pendingText.length <= hold) return visible;
      visible += state.pendingText.slice(0, -hold);
      state.pendingText = state.pendingText.slice(-hold);
      return cleanVisibleText(visible);
    }

    visible += state.pendingText.slice(0, start);
    state.pendingText = state.pendingText.slice(start + matched.start.length);
    state.inHiddenChannel = true;
    state.hiddenMarker = matched;
  }

  return cleanVisibleText(visible);
}

function flushVisibleText(state) {
  if (state.inHiddenChannel) {
    state.capturedReasoning += state.pendingText;
    state.pendingText = "";
    return "";
  }
  const text = cleanVisibleText(state.pendingText);
  state.pendingText = "";
  return text;
}

/// Drains reasoning captured from hidden `content` spans since the last call.
function takeCapturedReasoning(state) {
  const captured = state.capturedReasoning;
  state.capturedReasoning = "";
  return captured;
}

function buildToolNamespaceMap(tools) {
  // Map each flattened namespace sub-tool -> its namespace, so function_calls
  // can be emitted with the namespace codex needs to route them (router.rs).
  const map = {};
  for (const tool of tools || []) {
    if (tool && tool.type === "namespace" && Array.isArray(tool.tools)) {
      for (const sub of tool.tools) {
        if (sub && sub.type === "function" && sub.name) map[sub.name] = tool.name;
      }
    }
  }
  return map;
}

function ensureToolCall(state, res, toolDelta) {
  finishReasoning(state, res);
  const index = toolDelta.index ?? 0;
  if (!state.toolCalls.has(index)) {
    const item = {
      id: `fc_${Date.now()}_${index}`,
      type: "function_call",
      status: "in_progress",
      call_id: toolDelta.id || `call_${Date.now()}_${index}`,
      name: toolDelta.function?.name || "",
      arguments: "",
    };
    state.toolCalls.set(index, item);
    sendSse(res, "response.output_item.added", {
      type: "response.output_item.added",
      output_index: state.output.length + state.toolCalls.size - 1,
      item,
    });
  }
  const item = state.toolCalls.get(index);
  if (toolDelta.id) item.call_id = toolDelta.id;
  if (toolDelta.function?.name) item.name = toolDelta.function.name;
  if (state.toolNamespaces && item.name && state.toolNamespaces[item.name]) {
    item.namespace = state.toolNamespaces[item.name];
  }
  return item;
}

function emitToolDelta(state, res, toolDelta) {
  const item = ensureToolCall(state, res, toolDelta);
  const delta = toolDelta.function?.arguments || "";
  if (!delta) return;
  item.arguments += delta;
  sendSse(res, "response.function_call_arguments.delta", {
    type: "response.function_call_arguments.delta",
    item_id: item.id,
    output_index: state.output.length + [...state.toolCalls.values()].indexOf(item),
    delta,
  });
}

function finishMessage(state, res) {
  if (!state.messageStarted) return;
  sendSse(res, "response.output_text.done", {
    type: "response.output_text.done",
    item_id: state.messageId,
    output_index: state.output.length,
    content_index: 0,
    text: state.text,
  });
  const part = { type: "output_text", text: state.text, annotations: [] };
  sendSse(res, "response.content_part.done", {
    type: "response.content_part.done",
    item_id: state.messageId,
    output_index: state.output.length,
    content_index: 0,
    part,
  });
  const item = { id: state.messageId, type: "message", status: "completed", role: "assistant", content: [part] };
  sendSse(res, "response.output_item.done", { type: "response.output_item.done", output_index: state.output.length, item });
  state.output.push(item);
}

// Small tool-parsers (e.g. Qwen3 qwen3_xml) occasionally emit tool-call
// arguments with invalid JSON — most commonly a missing comma between a value
// and the next key ({"x": 500 "y": 500}). That breaks BOTH local tool execution
// and the follow-up request (the gateway re-parses the replayed tool_call and
// returns "Expecting ',' delimiter"). Repair to valid JSON when we can; only
// return the repaired string if it actually parses, so we never make it worse.
function repairJsonArgs(s) {
  if (typeof s !== "string") return "{}";
  let t = s.trim();
  if (!t) return "{}";
  try { JSON.parse(t); return t; } catch {}
  // Strip code fences / prose around the JSON object or array.
  const start = t.search(/[{[]/);
  if (start > 0) t = t.slice(start);
  const end = Math.max(t.lastIndexOf("}"), t.lastIndexOf("]"));
  if (end >= 0 && end < t.length - 1) t = t.slice(0, end + 1);
  // Common per-fix transforms (guarded: we only keep a result that parses).
  const commas = (x) => x
    // missing comma between a value and the next key:  500 "y" -> 500, "y"
    .replace(/(true|false|null|\d|"|\}|\])\s+(")/g, "$1, $2")
    // trailing comma before a close:  [1,2,] -> [1,2]
    .replace(/,\s*([}\]])/g, "$1");
  // Python-style literals -> JSON (whole-word):  True/False/None
  const pyLit = (x) => x
    .replace(/\bTrue\b/g, "true").replace(/\bFalse\b/g, "false").replace(/\bNone\b/g, "null");
  const candidates = [t, commas(t), pyLit(t), commas(pyLit(t))];
  // All single quotes and no doubles -> swap (Python-dict style).
  if (!t.includes('"') && t.includes("'")) candidates.push(commas(pyLit(t.replace(/'/g, '"'))));
  for (const c of candidates) {
    try { JSON.parse(c); if (c !== s) console.error(`[toolrepair] fixed ${JSON.stringify(s).slice(0, 120)} -> ${c.slice(0, 120)}`); return c; }
    catch {}
  }
  console.error(`[toolrepair] UNREPAIRABLE tool args: ${JSON.stringify(s).slice(0, 200)}`);
  return t;
}

function finishToolCalls(state, res) {
  for (const item of state.toolCalls.values()) {
    item.arguments = repairJsonArgs(item.arguments);
    const outputIndex = state.output.length;
    sendSse(res, "response.function_call_arguments.done", {
      type: "response.function_call_arguments.done",
      item_id: item.id,
      output_index: outputIndex,
      arguments: item.arguments,
    });
    const doneItem = { ...item, status: "completed" };
    sendSse(res, "response.output_item.done", { type: "response.output_item.done", output_index: outputIndex, item: doneItem });
    state.output.push(doneItem);
  }
}

function newResponseState(model) {
  return {
    responseId: `resp_${Date.now()}`,
    model,
    messageId: `msg_${Date.now()}`,
    messageStarted: false,
    text: "",
    output: [],
    toolCalls: new Map(),
    usage: null,
    inHiddenChannel: false,
    hiddenMarker: null,
    pendingText: "",
    capturedReasoning: "",
    reasoningId: `rs_${Date.now()}`,
    reasoningStarted: false,
    reasoningFinished: false,
    reasoningText: "",
  };
}

/// Maps upstream chat-completions usage (with provider-specific cache fields)
/// onto Responses-API usage, including cached and reasoning token details so
/// the client's TokenUsage picks them up.
function usageFromChunk(usage) {
  const promptDetails = usage.prompt_tokens_details || usage.input_tokens_details || {};
  const completionDetails = usage.completion_tokens_details || usage.output_tokens_details || {};
  const cachedTokens = promptDetails.cached_tokens
    || usage.prompt_cache_hit_tokens
    || usage.cache_read_input_tokens
    || 0;
  const reasoningTokens = completionDetails.reasoning_tokens || usage.reasoning_tokens || 0;
  const inputTokens = usage.prompt_tokens || usage.input_tokens || 0;
  const outputTokens = usage.completion_tokens || usage.output_tokens || 0;
  return {
    input_tokens: inputTokens,
    input_tokens_details: { cached_tokens: cachedTokens },
    output_tokens: outputTokens,
    output_tokens_details: { reasoning_tokens: reasoningTokens },
    // Some OpenAI-compatible servers omit total_tokens. Falling back to 0 would
    // make the client believe the context is near-empty, so auto-compaction
    // never triggers and the conversation grows until the model rejects it.
    // Derive it from input+output whenever the upstream doesn't supply it.
    total_tokens: usage.total_tokens || (inputTokens + outputTokens),
  };
}

/// Extracts a reasoning delta from a chat-completions streaming delta,
/// covering reasoning_content (vLLM/DeepSeek/Kimi), reasoning (some routers),
/// and OpenRouter-style reasoning_details.
function reasoningDeltaFromChunk(delta) {
  if (typeof delta?.reasoning_content === "string" && delta.reasoning_content) return delta.reasoning_content;
  if (typeof delta?.reasoning === "string" && delta.reasoning) return delta.reasoning;
  if (Array.isArray(delta?.reasoning_details)) {
    return delta.reasoning_details
      .map((detail) => detail?.text || detail?.summary || "")
      .filter(Boolean)
      .join("");
  }
  return "";
}

// Reasoning must be emitted as its own Responses output item, opened before and
// closed before the assistant message item: the client tracks a single active
// item, so interleaving deltas across items would be dropped or crash it.
function reasoningItemShell(state, status) {
  return {
    id: state.reasoningId,
    type: "reasoning",
    status,
    summary: state.reasoningText
      ? [{ type: "summary_text", text: state.reasoningText }]
      : [],
    encrypted_content: null,
  };
}

function emitReasoningStart(state, res) {
  if (state.reasoningStarted) return;
  state.reasoningStarted = true;
  sendSse(res, "response.output_item.added", {
    type: "response.output_item.added",
    output_index: state.output.length,
    item: reasoningItemShell(state, "in_progress"),
  });
  sendSse(res, "response.reasoning_summary_part.added", {
    type: "response.reasoning_summary_part.added",
    item_id: state.reasoningId,
    output_index: state.output.length,
    summary_index: 0,
  });
}

function emitReasoningDelta(state, res, delta) {
  if (!delta || state.reasoningFinished) {
    // Late reasoning after the message opened still lands in the transcript
    // total via state.reasoningText, it just cannot stream live any more.
    state.reasoningText += delta || "";
    return;
  }
  emitReasoningStart(state, res);
  state.reasoningText += delta;
  sendSse(res, "response.reasoning_summary_text.delta", {
    type: "response.reasoning_summary_text.delta",
    item_id: state.reasoningId,
    output_index: state.output.length,
    summary_index: 0,
    delta,
  });
}

function finishReasoning(state, res) {
  if (!state.reasoningStarted || state.reasoningFinished) {
    state.reasoningFinished = true;
    return;
  }
  state.reasoningFinished = true;
  const item = reasoningItemShell(state, "completed");
  sendSse(res, "response.output_item.done", {
    type: "response.output_item.done",
    output_index: state.output.length,
    item,
  });
  state.output.push(item);
}

async function collectChatToResponse(upstream, model) {
  const state = newResponseState(model);
  const reader = upstream.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  while (true) {
    const { value, done } = await readWithTimeout(reader, streamInactivityTimeoutMs);
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const events = buffer.split(/\n\n/);
    buffer = events.pop() || "";
    for (const eventText of events) {
      const dataLines = eventText
        .split(/\n/)
        .filter((line) => line.startsWith("data:"))
        .map((line) => line.slice(5).trim());
      if (!dataLines.length) continue;
      const data = dataLines.join("\n");
      if (data === "[DONE]") continue;
      let chunk;
      try {
        chunk = JSON.parse(data);
      } catch {
        continue;
      }
      if (chunk.usage) state.usage = usageFromChunk(chunk.usage);
      const choice = chunk.choices?.[0];
      const delta = choice?.delta || {};
      state.reasoningText += reasoningDeltaFromChunk(delta);
      state.text += visibleTextDelta(state, delta.content || "");
      state.reasoningText += takeCapturedReasoning(state);
      for (const toolDelta of delta.tool_calls || []) {
        const index = toolDelta.index ?? 0;
        if (!state.toolCalls.has(index)) {
          state.toolCalls.set(index, {
            id: `fc_${Date.now()}_${index}`,
            type: "function_call",
            status: "in_progress",
            call_id: toolDelta.id || `call_${Date.now()}_${index}`,
            name: toolDelta.function?.name || "",
            arguments: "",
          });
        }
        const item = state.toolCalls.get(index);
        if (toolDelta.id) item.call_id = toolDelta.id;
        if (toolDelta.function?.name) item.name = toolDelta.function.name;
        item.arguments += toolDelta.function?.arguments || "";
      }
    }
  }

  state.text += flushVisibleText(state);
  state.reasoningText += takeCapturedReasoning(state);
  return state;
}

async function readWithTimeout(reader, timeoutMs) {
  let timeout;
  try {
    return await Promise.race([
      reader.read(),
      new Promise((_, reject) => {
        timeout = setTimeout(() => reject(new Error(`Upstream stream timed out after ${timeoutMs}ms without data`)), timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timeout);
  }
}

function emitCollectedResponse(state, res) {
  sseHeaders(res);
  sendSse(res, "response.created", {
    type: "response.created",
    response: responseShell(state.responseId, state.model),
  });

  if (state.reasoningText && !state.reasoningStarted) {
    emitReasoningStart(state, res);
    sendSse(res, "response.reasoning_summary_text.delta", {
      type: "response.reasoning_summary_text.delta",
      item_id: state.reasoningId,
      output_index: state.output.length,
      summary_index: 0,
      delta: state.reasoningText,
    });
    finishReasoning(state, res);
  }

  if (state.text) {
    const item = { id: state.messageId, type: "message", status: "in_progress", role: "assistant", content: [] };
    sendSse(res, "response.output_item.added", { type: "response.output_item.added", output_index: state.output.length, item });
    sendSse(res, "response.content_part.added", {
      type: "response.content_part.added",
      item_id: state.messageId,
      output_index: state.output.length,
      content_index: 0,
      part: { type: "output_text", text: "", annotations: [] },
    });
    sendSse(res, "response.output_text.delta", {
      type: "response.output_text.delta",
      item_id: state.messageId,
      output_index: state.output.length,
      content_index: 0,
      delta: state.text,
    });
    sendSse(res, "response.output_text.done", {
      type: "response.output_text.done",
      item_id: state.messageId,
      output_index: state.output.length,
      content_index: 0,
      text: state.text,
    });
    const part = { type: "output_text", text: state.text, annotations: [] };
    sendSse(res, "response.content_part.done", {
      type: "response.content_part.done",
      item_id: state.messageId,
      output_index: state.output.length,
      content_index: 0,
      part,
    });
    const doneItem = { id: state.messageId, type: "message", status: "completed", role: "assistant", content: [part] };
    sendSse(res, "response.output_item.done", { type: "response.output_item.done", output_index: state.output.length, item: doneItem });
    state.output.push(doneItem);
  }

  for (const item of state.toolCalls.values()) {
    item.arguments = repairJsonArgs(item.arguments);
    const outputIndex = state.output.length;
    sendSse(res, "response.output_item.added", {
      type: "response.output_item.added",
      output_index: outputIndex,
      item,
    });
    if (item.arguments) {
      sendSse(res, "response.function_call_arguments.delta", {
        type: "response.function_call_arguments.delta",
        item_id: item.id,
        output_index: outputIndex,
        delta: item.arguments,
      });
    }
    sendSse(res, "response.function_call_arguments.done", {
      type: "response.function_call_arguments.done",
      item_id: item.id,
      output_index: outputIndex,
      arguments: item.arguments,
    });
    const doneItem = { ...item, status: "completed" };
    sendSse(res, "response.output_item.done", { type: "response.output_item.done", output_index: outputIndex, item: doneItem });
    state.output.push(doneItem);
  }

  sendSse(res, "response.completed", {
    type: "response.completed",
    response: responseShell(state.responseId, state.model, "completed", state.output, state.usage),
  });
  res.end();
}

/// Streams one upstream chat-completions response into `state`, emitting
/// Responses-API deltas (reasoning, text, tool args) to the client as they
/// arrive. Does NOT finish items or complete the response, so callers can
/// append further attempts (Agent Check retries) into the same message.
async function streamUpstreamIntoState(upstream, res, state) {
  const reader = upstream.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  while (true) {
    const { value, done } = await readWithTimeout(reader, streamInactivityTimeoutMs);
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const events = buffer.split(/\n\n/);
    buffer = events.pop() || "";
    for (const eventText of events) {
      const dataLines = eventText
        .split(/\n/)
        .filter((line) => line.startsWith("data:"))
        .map((line) => line.slice(5).trim());
      if (!dataLines.length) continue;
      const data = dataLines.join("\n");
      if (data === "[DONE]") continue;
      let chunk;
      try {
        chunk = JSON.parse(data);
      } catch {
        continue;
      }
      if (chunk.usage) state.usage = usageFromChunk(chunk.usage);
      const choice = chunk.choices?.[0];
      const delta = choice?.delta || {};
      emitReasoningDelta(state, res, reasoningDeltaFromChunk(delta));
      const visibleDelta = visibleTextDelta(state, delta.content || "");
      emitReasoningDelta(state, res, takeCapturedReasoning(state));
      emitTextDelta(state, res, visibleDelta);
      for (const toolDelta of delta.tool_calls || []) emitToolDelta(state, res, toolDelta);
    }
  }

  emitReasoningDelta(state, res, takeCapturedReasoning(state));
  emitTextDelta(state, res, flushVisibleText(state));
}

/// Closes all in-progress items and completes the streamed response.
function finishStreamedResponse(state, res) {
  finishReasoning(state, res);
  finishMessage(state, res);
  finishToolCalls(state, res);
  if (!state.output.length) {
    emitTextDelta(state, res, "");
    finishMessage(state, res);
  }
  sendSse(res, "response.completed", {
    type: "response.completed",
    response: responseShell(state.responseId, state.model, "completed", state.output, state.usage),
  });
  res.end();
}

function latestUserText(body) {
  for (let index = (body.input || []).length - 1; index >= 0; index--) {
    const item = body.input[index];
    if (item?.type === "message" && item.role === "user") {
      return contentText(item.content);
    }
  }
  return "";
}

function parseAgentCheck(text) {
  const trimmed = String(text || "").trim();
  const jsonText = trimmed.match(/\{[\s\S]*\}/)?.[0] || trimmed;
  try {
    const parsed = JSON.parse(jsonText);
    return {
      answered: Boolean(parsed.answered),
      direction: typeof parsed.direction === "string" ? parsed.direction : "",
    };
  } catch {
    // Judges (especially weak models) sometimes wrap or mangle the JSON.
    // Fall back to a regex scan before giving up, and mark unparseable
    // verdicts so the caller can log them instead of silently passing.
    const answeredMatch = trimmed.match(/"answered"\s*:\s*(true|false)/i);
    if (answeredMatch) {
      const directionMatch = trimmed.match(/"direction"\s*:\s*"([^"]*)"/i);
      return {
        answered: answeredMatch[1].toLowerCase() === "true",
        direction: directionMatch ? directionMatch[1] : "",
      };
    }
    return { answered: true, direction: "", unparseable: true };
  }
}

function parseOnOff(value, fallback) {
  switch (String(value || "").trim().toLowerCase()) {
    case "on":
    case "true":
    case "1":
    case "yes":
    case "enabled":
    case "enable":
      return true;
    case "off":
    case "false":
    case "0":
    case "no":
    case "disabled":
    case "disable":
      return false;
    case "":
      return fallback;
    default:
      return fallback;
  }
}

function agentCheckEnabled() {
  let enabled = parseOnOff(
    (process.env.RETRACE_AGENT_CHECK_ENABLED || process.env.CODEXOS_AGENT_CHECK_ENABLED) || (process.env.RETRACE_AGENT_CHECK || process.env.CODEXOS_AGENT_CHECK),
    false,
  );
  try {
    if (fs.existsSync(agentCheckStateFile)) {
      enabled = parseOnOff(fs.readFileSync(agentCheckStateFile, "utf8"), enabled);
    }
  } catch {
    return enabled;
  }
  return enabled;
}

function agentCheckShowNotes() {
  return parseOnOff((process.env.RETRACE_AGENT_CHECK_SHOW_NOTES || process.env.CODEXOS_AGENT_CHECK_SHOW_NOTES), false);
}

function looksLikeLeakedToolCall(text) {
  const t = String(text || "").trim();
  if (!t) return false;
  // A tool-call token, or a text-encoded tool call the model emitted instead of
  // a structured tool_call (Gemma-4 tool-call leak, e.g. `call:write_stdin{...}`).
  return /<\|?\s*tool_call\s*\|?>/i.test(t) || /(^|[\s>])call:[a-z0-9_]+\s*\{/i.test(t);
}

function shouldRunAgentCheck(state) {
  // Eligible whenever the model produced no tool call. This includes an EMPTY
  // final answer, which is itself a non-answer that should be retried rather
  // than silently ending the turn.
  return state.toolCalls.size === 0;
}

async function runAgentCheck(body, state) {
  const checkBody = {
    ...body,
    tools: [],
    tool_choice: "none",
    max_output_tokens: 220,
    input: [
      {
        type: "message",
        role: "system",
        content: `You are Agent Check. Decide if the assistant's draft fully answers the user's latest request.
	Return JSON only with this exact shape: {"answered":true|false,"direction":"..."}.
	Use answered=false when the draft is only a progress update, promises future work, says it will inspect/read/check, lacks the requested final answer, or stops before using an obvious next step.
	If answered=false and the draft itself points to a next step, put that next step in direction. Otherwise provide a concise new approach.`,
      },
      ...(body.input || []),
      {
        type: "message",
        role: "assistant",
        content: stripProxyArtifacts(state.text).trim(),
      },
      {
        type: "message",
        role: "user",
        content: `Agent Check: using the full conversation and initial instructions above, decide whether the immediately preceding assistant draft fully completed the latest user request. Return JSON only.`,
      },
    ],
  };

  const upstream = await upstreamChat(checkBody, false);
  if (!upstream.response.ok) {
    console.error(`[agentcheck] judge call failed (${upstream.response.status}); passing by default`);
    return { answered: true, direction: "" };
  }
  const checkState = await collectChatToResponse(upstream.response, body.model);
  const verdict = parseAgentCheck(checkState.text);
  if (verdict.unparseable) {
    console.error(`[agentcheck] judge verdict unparseable; passing by default: ${JSON.stringify(String(checkState.text || "").slice(0, 160))}`);
  }
  return verdict;
}

function bodyWithAgentCheckFeedback(body, state, check) {
  return {
    ...body,
    input: [
      ...(body.input || []),
      {
        type: "message",
        role: "assistant",
        content: stripProxyArtifacts(state.text),
      },
      {
        type: "message",
        role: "user",
        content: `Agent Check says the previous draft did not fully answer the request. Continue now and complete the task. Direction: ${check.direction || "Use a new approach and provide the missing answer."}`,
      },
    ],
  };
}

function prependAgentCheckRetryNote(state, direction) {
  const note = `Agent Check: retrying because the previous answer was incomplete. Direction: ${direction || "Use a new approach and provide the missing answer."}\n\n`;
  state.text = note + state.text;
}

function prependAgentCheckBlockedNote(state, direction) {
  const note = `Agent Check: retry limit reached; the retry still did not complete the task. Last direction: ${direction || "Use a new approach and provide the missing answer."}\n\n`;
  state.text = note + state.text;
}

async function handleResponses(req, res, body) {
  if (!modelIsAllowed(body.model)) {
    json(res, 400, {
      error: {
        message: `Model ${body.model || "(missing)"} is not enabled in Retrace`,
        type: "model_not_enabled",
      },
    });
    return;
  }

  let upstream = await upstreamChat(body, true);
  if (!upstream.response.ok && hasRequestTools(body)) {
    upstream = await upstreamChat(body, false);
  }
  if (!upstream.response.ok) {
    const errText = upstream.text || `Upstream returned ${upstream.response.status}`;
    // Normalize a context-length overflow from ANY provider into the code the
    // client recognizes (context_length_exceeded), emitted as an SSE
    // response.failed. Without this the client cannot classify the overflow, so
    // it neither marks the context full nor auto-compacts on the next turn and
    // the session gets stuck re-overflowing. With it, the overflow marks the
    // context full so the next turn compacts and recovers.
    if (looksLikeContextLengthError(errText) && !res.headersSent) {
      const rid = `resp_ctx_${Math.random().toString(36).slice(2)}`;
      sseHeaders(res);
      sendSse(res, "response.created", {
        type: "response.created",
        response: responseShell(rid, turnBody.model, "in_progress"),
      });
      sendSse(res, "response.failed", {
        type: "response.failed",
        response: {
          ...responseShell(rid, turnBody.model, "failed"),
          error: { code: "context_length_exceeded", message: errText },
        },
      });
      res.end();
      return;
    }
    json(res, upstream.response.status, {
      error: { message: errText, type: "upstream_error" },
    });
    return;
  }

  // Always stream: attempt 1 is forwarded live. When Agent Check is enabled and
  // judges the streamed draft incomplete, each retry streams as a continuation
  // of the same message (separated by a note) until the check passes or the
  // retry budget is exhausted.
  const state = newResponseState(body.model);
  state.toolNamespaces = buildToolNamespaceMap(body.tools);
  sseHeaders(res);
  sendSse(res, "response.created", {
    type: "response.created",
    response: responseShell(state.responseId, state.model),
  });
  await streamUpstreamIntoState(upstream.response, res, state);

  const maxAgentCheckRetries = agentCheckEnabled()
    ? Number((process.env.RETRACE_AGENT_CHECK_RETRIES || process.env.CODEXOS_AGENT_CHECK_RETRIES) || "50")
    : 0;

  if (agentCheckEnabled()) {
    console.error(
      `[agentcheck] model=${body.model} enabled=true eligible=${shouldRunAgentCheck(state)} (text=${Boolean(state.text.trim())} toolCalls=${state.toolCalls.size})`,
    );
  }

  let agentCheckRan = false;
  let agentCheckPassed = false;
  for (let attempt = 0; attempt < maxAgentCheckRetries; attempt++) {
    if (!shouldRunAgentCheck(state)) break;
    agentCheckRan = true;

    // A leaked/malformed tool call is never a valid final answer. Force a retry
    // without spending a judge call so the turn never ends on that garbage.
    let check;
    if (!state.text.trim()) {
      check = {
        answered: false,
        direction:
          "Your previous response was empty. Provide the completed final answer to the user's request now.",
      };
      console.error(`[agentcheck] attempt ${attempt + 1}: empty draft, forcing retry`);
    } else if (looksLikeLeakedToolCall(state.text)) {
      check = {
        answered: false,
        direction:
          "Your previous output was a malformed or leaked tool call, not an answer. Either issue a proper tool call now, or give the completed final answer to the user's request.",
      };
      console.error(`[agentcheck] attempt ${attempt + 1}: leaked-tool-call draft, forcing retry`);
    } else {
      check = await runAgentCheck(body, state);
      console.error(
        `[agentcheck] attempt ${attempt + 1}: answered=${check.answered}${check.answered ? "" : ` direction=${JSON.stringify((check.direction || "").slice(0, 120))}`}`,
      );
    }
    if (check.answered) {
      agentCheckPassed = true;
      break;
    }

    const retryBody = bodyWithAgentCheckFeedback(body, state, {
      ...check,
      direction: `${check.direction || "Use a new approach and provide the missing answer."} Do not answer with a progress update. Either call the needed tool now or provide the completed final answer.`,
    });
    let retry = await upstreamChat(retryBody, true);
    if (!retry.response.ok && hasRequestTools(retryBody)) {
      retry = await upstreamChat(retryBody, false);
    }
    if (!retry.response.ok) break;

    // Always show the full Agent Check banner on retry so the user can see it
    // working. (SHOW_NOTES no longer gates the retry banner.)
    const separator = `\n\nAgent Check: the draft above was incomplete; continuing. Direction: ${check.direction || "complete the task"}\n\n`;
    emitTextDelta(state, res, separator);
    await streamUpstreamIntoState(retry.response, res, state);
  }

  // Always-visible tag so the user can see Agent Check ran (the spinner shows
  // the "working" activity during the check itself). If a retry produced a
  // proper tool call, the turn continues with tool execution — that is
  // progress, not exhaustion, so no verdict tag yet (the eventual final
  // answer gets checked again on the next round-trip).
  if (agentCheckRan && state.toolCalls.size === 0) {
    emitTextDelta(
      state,
      res,
      agentCheckPassed ? "\n\n_✓ Agent Check_" : "\n\n_⚠ Agent Check: retries exhausted_",
    );
  }

  finishStreamedResponse(state, res);
}

async function proxyModels(_req, res) {
  json(res, 200, localModelCatalog());
}

const server = http.createServer((req, res) => {
  const chunks = [];
  req.on("data", (chunk) => chunks.push(chunk));
  req.on("end", async () => {
    try {
      if (req.method === "GET" && req.url?.endsWith("/models")) {
        await proxyModels(req, res);
        return;
      }
      if (req.method !== "POST" || !req.url?.endsWith("/responses")) {
        json(res, 404, { error: { message: "Not found" } });
        return;
      }
      const body = JSON.parse(Buffer.concat(chunks).toString("utf8") || "{}");
      await handleResponses(req, res, body);
    } catch (error) {
      json(res, 500, { error: { message: error.stack || error.message || String(error), type: "proxy_error" } });
    }
  });
});

server.listen(port, host, () => {
  const address = server.address();
  const actualPort = typeof address === "object" && address ? address.port : port;
  if (readyFile) fs.writeFileSync(readyFile, `${actualPort}\n`, { mode: 0o600 });
  console.error(`retrace proxy listening on ${host}:${actualPort}`);
});
