#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const homeDir = process.env.HOME;
const codexosHome = (process.env.RETRACE_HOME || process.env.CODEXOS_HOME) || path.join(homeDir, ".retrace");
const registryPath = (process.env.RETRACE_REGISTRY_JSON || process.env.CODEXOS_REGISTRY_JSON) || path.join(codexosHome, "registry.json");
const catalogPath = (process.env.RETRACE_MODEL_CATALOG_JSON || process.env.CODEXOS_MODEL_CATALOG_JSON) || path.join(codexosHome, "models.json");
const defaultKeyPath = path.join(codexosHome, "api_key");

function usage() {
  console.log(`retrace-admin

Usage:
  retrace-admin doctor [--online]
  retrace-admin registry show [--json]
  retrace-admin provider list [--json]
  retrace-admin provider add <id> --base-url <url> [--name <name>] [--api-key-file <path>|--env-key <name>] [--default-context <tokens>]
  retrace-admin provider connect <id> --base-url <url> (--stdin|--api-key-file <path>|--env-key <name>) [--name <name>] [--enable] [--probe] [--probe-limit <n>]
  retrace-admin provider remove <id>
  retrace-admin auth set <provider> (--stdin|--api-key-file <path>|--env-key <name>)
  retrace-admin models list [--all] [--json]
  retrace-admin models refresh [--provider <id>] [--enable] [--probe] [--probe-limit <n>]
  retrace-admin models probe <model>... [--provider <id>]
  retrace-admin models enable <model>...
  retrace-admin models disable <model>...
  retrace-admin models remove <model>... [--provider <id>]
  retrace-admin models set <model> [--context <tokens>] [--effective-percent <1-100>] [--output <tokens|none|default>] [--thinking <on|off|auto|level>] [--normalize-system <ascii|off>] [--provider <id>] [--upstream-model <id>] [--display-name <name>]
  retrace-admin catalog build

Notes:
  - API keys are never printed.
  - Use --stdin to set a key without putting it in shell history:
      printf '%s' "$KEY" | retrace-admin auth set <provider-id> --stdin
`);
}

function expandHome(value) {
  if (!value) return value;
  if (value === "~") return homeDir;
  if (value.startsWith("~/")) return path.join(homeDir, value.slice(2));
  return value;
}

function normalizeBaseUrl(value) {
  let text = String(value || "").trim().replace(/['"]/g, "").replace(/\/+$/, "");
  text = text.replace(/\/(chat\/completions|completions|messages|models)$/i, "");
  if (!/^https?:\/\//i.test(text)) {
    const local = /(\d+\.\d+\.\d+\.\d+|localhost|\.local|:\d{2,5})/.test(text);
    text = `${local ? "http" : "https"}://${text}`;
  }
  if (!/\/v\d(?:\/|$)/i.test(text)) text += "/v1";
  return text.replace(/\/+$/, "");
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}

function fileMode(file, fallback = 0o600) {
  try {
    return fs.statSync(file).mode & 0o777;
  } catch {
    return fallback;
  }
}

function writeJsonAtomic(file, value, mode = 0o600) {
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, `${JSON.stringify(value, null, 2)}\n`, { mode });
  fs.renameSync(tmp, file);
  fs.chmodSync(file, mode);
}

function backupFile(file) {
  if (!fs.existsSync(file)) return "";
  const stamp = new Date().toISOString().replace(/[-:]/g, "").replace(/\.\d+Z$/, "Z");
  const backup = `${file}.bak-admin-${stamp}`;
  fs.copyFileSync(file, backup);
  fs.chmodSync(backup, fileMode(file));
  return backup;
}

function defaultRegistry() {
  // A fresh install has no providers or models. Users connect their own with
  // `retrace` -> /model -> "Add custom model", or `retrace-admin provider connect`.
  return {
    version: 1,
    defaultProvider: "",
    defaultModel: "",
    providers: {},
    modelOrder: [],
    models: {},
  };
}

function ensureShape(registry) {
  registry.version ||= 1;
  registry.providers ||= {};
  if (!registry.defaultProvider || !registry.providers[registry.defaultProvider]) {
    registry.defaultProvider = Object.keys(registry.providers)[0] || "";
  }
  registry.models ||= {};
  registry.modelOrder ||= Object.keys(registry.models);
  return registry;
}

function loadRegistry() {
  if (!fs.existsSync(registryPath)) {
    const registry = defaultRegistry();
    writeJsonAtomic(registryPath, registry);
    return registry;
  }
  return ensureShape(readJson(registryPath));
}

function saveRegistry(registry) {
  writeJsonAtomic(registryPath, ensureShape(registry), fileMode(registryPath));
}

function redactRegistry(registry) {
  const clone = JSON.parse(JSON.stringify(registry));
  for (const provider of Object.values(clone.providers || {})) {
    if (provider.apiKey) provider.apiKey = "[redacted]";
    if (provider.apiKeyFile) provider.apiKeyFile = provider.apiKeyFile;
    if (provider.envKey) provider.envKey = provider.envKey;
  }
  return clone;
}

function parseArgs(rawArgs) {
  const result = { _: [] };
  for (let i = 0; i < rawArgs.length; i++) {
    const arg = rawArgs[i];
    if (!arg.startsWith("--")) {
      result._.push(arg);
      continue;
    }
    const eq = arg.indexOf("=");
    if (eq !== -1) {
      result[arg.slice(2, eq)] = arg.slice(eq + 1);
      continue;
    }
    const key = arg.slice(2);
    const next = rawArgs[i + 1];
    if (!next || next.startsWith("--")) {
      result[key] = true;
    } else {
      result[key] = next;
      i++;
    }
  }
  return result;
}

function requireProvider(registry, providerId) {
  const provider = registry.providers?.[providerId];
  if (!provider) throw new Error(`Provider not found: ${providerId}`);
  return provider;
}

function providerKey(provider) {
  if (provider.envKey && process.env[provider.envKey]) return process.env[provider.envKey].trim();
  if (provider.apiKeyFile) {
    const keyFile = expandHome(provider.apiKeyFile);
    if (fs.existsSync(keyFile)) return fs.readFileSync(keyFile, "utf8").trim();
  }
  return "";
}

function keyStatus(provider) {
  if (provider.envKey && process.env[provider.envKey]) return `env:${provider.envKey}`;
  if (provider.apiKeyFile && fs.existsSync(expandHome(provider.apiKeyFile))) return `file:${provider.apiKeyFile}`;
  return "missing";
}

function timeoutSignal(ms) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), ms);
  return { signal: controller.signal, cancel: () => clearTimeout(timeout) };
}

function parseModelIds(json) {
  const rows = Array.isArray(json?.data)
    ? json.data
    : Array.isArray(json?.models)
      ? json.models
      : Array.isArray(json)
        ? json
        : [];
  return rows
    .map((row) => row?.id || row?.slug || row?.name || row?.model || (typeof row === "string" ? row : ""))
    .map(String)
    .filter(Boolean);
}

function dialectOrderForKey(key) {
  return /^sk-ant-/i.test(key) ? ["anthropic", "openai"] : ["openai", "anthropic"];
}

function kindFromUrl(url, dialect) {
  const lower = String(url || "").toLowerCase();
  if (lower.includes("anthropic")) return "anthropic";
  if (lower.includes("openai.com")) return "openai";
  return dialect === "anthropic" ? "anthropic" : "openai-compatible";
}

function modelListRequest(baseUrl, key, dialect) {
  if (dialect === "anthropic") {
    return {
      url: `${baseUrl}/models`,
      headers: { "x-api-key": key, "anthropic-version": "2023-06-01" },
    };
  }
  return {
    url: `${baseUrl}/models`,
    headers: { Authorization: `Bearer ${key}` },
  };
}

async function fetchJson(url, options = {}, timeoutMs = 12000) {
  const { signal, cancel } = timeoutSignal(timeoutMs);
  try {
    const response = await fetch(url, { ...options, signal });
    const text = await response.text();
    let data = {};
    try {
      data = text ? JSON.parse(text) : {};
    } catch {
      data = { raw: text };
    }
    return { ok: response.ok, status: response.status, data, text };
  } finally {
    cancel();
  }
}

async function probeProviderEndpoint(rawUrl, key) {
  const base = normalizeBaseUrl(rawUrl);
  const root = base.replace(/\/v\d.*$/i, "");
  const candidates = Array.from(new Set([base, `${root}/v1`, root, `${base}/openai/v1`]));
  let lastError = "no endpoint responded";

  for (const candidate of candidates) {
    for (const dialect of dialectOrderForKey(key)) {
      const request = modelListRequest(candidate, key, dialect);
      try {
        const response = await fetchJson(request.url, { headers: request.headers }, 9000);
        if (!response.ok) {
          lastError = `${request.url} -> HTTP ${response.status}`;
          continue;
        }
        const models = parseModelIds(response.data);
        if (!models.length) {
          lastError = `${request.url} -> no models listed`;
          continue;
        }
        return {
          resolved: candidate,
          providerKind: kindFromUrl(candidate, dialect),
          authStyle: dialect === "anthropic" ? "x-api-key" : "bearer",
          dialect,
          models,
        };
      } catch (error) {
        lastError = `${request.url} -> ${error?.name === "AbortError" ? "timeout" : error?.message || String(error)}`;
      }
    }
  }

  throw new Error(`could not resolve provider (${lastError})`);
}

const requestRecipes = [
  { tokenParam: "max_tokens", temperature: true },
  { tokenParam: "max_tokens", temperature: false },
  { tokenParam: "max_completion_tokens", temperature: false },
];

// Thinking-activation formats across providers. Coverage map:
//   ctk_enable_thinking / enable_thinking  -> vLLM/SGLang local models, Qwen, DashScope
//   reasoning_effort                       -> OpenAI (gpt-5*/o-series), Grok, Gemini-openai-compat, MiniMax
//   reasoning_effort_obj                   -> OpenAI Responses-style routers, OpenRouter `reasoning: {effort}`
//   thinking_enabled                       -> Zhipu GLM-4.5+ (`thinking: {type: enabled|disabled}`)
//   reasoning_enabled                      -> generic routers (`reasoning: {enabled}`)
//   reasoning_max_tokens                   -> OpenRouter/gateway budget style (`reasoning: {max_tokens}`)
//   anthropic_thinking                     -> Anthropic Messages API (budget_tokens tiers = effort levels)
// DeepSeek (deepseek-reasoner), Kimi (kimi-*-thinking), MiniMax M-series and other
// always-thinking models are detected by the intrinsic pass (reasoning_content /
// <think> / reasoning_tokens in the base response) before these are tried.
const thinkMethods = [
  { id: "ctk_enable_thinking", body: { chat_template_kwargs: { enable_thinking: true } }, offBody: { chat_template_kwargs: { enable_thinking: false } }, levels: ["on"], dialect: "openai" },
  { id: "enable_thinking", body: { enable_thinking: true }, offBody: { enable_thinking: false }, levels: ["on"], dialect: "openai" },
  { id: "reasoning_effort", body: { reasoning_effort: "medium" }, offBody: { reasoning_effort: "none" }, levels: ["low", "medium", "high"], dialect: "openai" },
  { id: "reasoning_effort_obj", body: { reasoning: { effort: "medium" } }, offBody: { reasoning: { enabled: false } }, levels: ["low", "medium", "high"], dialect: "openai" },
  { id: "thinking_enabled", body: { thinking: { type: "enabled" } }, offBody: { thinking: { type: "disabled" } }, levels: ["on"], dialect: "openai" },
  { id: "reasoning_enabled", body: { reasoning: { enabled: true } }, offBody: { reasoning: { enabled: false } }, levels: ["on"], dialect: "openai" },
  { id: "reasoning_max_tokens", body: { reasoning: { max_tokens: 4096 } }, offBody: { reasoning: { enabled: false } }, levels: ["on"], dialect: "openai" },
  { id: "anthropic_thinking", body: { thinking: { type: "enabled", budget_tokens: 8192 } }, levels: ["low", "medium", "high"], dialect: "anthropic" },
];

// Effort strings worth testing when a method advertises graded levels. Every
// level the endpoint actually accepts AND that demonstrably produces reasoning
// gets stored; silently-ignored or rejected levels are dropped so /model only
// offers efforts that do something.
const effortLevelCandidates = ["minimal", "low", "medium", "high", "xhigh"];

// Anthropic expresses effort as a thinking budget; these tiers map the TUI's
// low/medium/high onto budget_tokens (max_tokens must exceed the budget).
const anthropicBudgetForLevel = { low: 2048, medium: 8192, high: 24576 };

const disableMethods = [
  { id: "ctk_enable_thinking_false", body: { chat_template_kwargs: { enable_thinking: false } } },
  { id: "enable_thinking_false", body: { enable_thinking: false } },
  { id: "thinking_disabled", body: { thinking: { type: "disabled" } } },
  { id: "reasoning_effort_none", body: { reasoning_effort: "none" } },
  { id: "reasoning_enabled_false", body: { reasoning: { enabled: false } } },
];

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

function isChittiGemma(modelId) {
  return /^chitti-|^gemma-|lilarest|cyankiwi/i.test(modelId || "");
}

function defaultRequestBody(modelId) {
  if (!isChittiGemma(modelId)) return {};
  return { chat_template_kwargs: { enable_thinking: /-(?:think|thinking)$/i.test(modelId) } };
}

function chatEndpoint(baseUrl, dialect) {
  return dialect === "anthropic" ? `${baseUrl}/messages` : `${baseUrl}/chat/completions`;
}

function chatHeaders(key, dialect) {
  if (dialect === "anthropic") return { "x-api-key": key, "anthropic-version": "2023-06-01", "content-type": "application/json" };
  return { Authorization: `Bearer ${key}`, "content-type": "application/json" };
}

function buildProbeBody(dialect, modelId, prompt, maxTokens, recipe, extra = {}) {
  if (dialect === "anthropic") {
    return deepMerge({ model: modelId, max_tokens: maxTokens, messages: [{ role: "user", content: prompt }] }, extra);
  }
  const body = { model: modelId, messages: [{ role: "user", content: prompt }], stream: false };
  body[recipe.tokenParam] = maxTokens;
  if (recipe.temperature) body.temperature = 0;
  return deepMerge(deepMerge(body, defaultRequestBody(modelId)), extra);
}

async function callChat(provider, key, modelId, prompt, maxTokens, recipe, extra = {}, timeoutMs = 20000) {
  const dialect = provider.dialect || "openai";
  const body = buildProbeBody(dialect, modelId, prompt, maxTokens, recipe, extra);
  const response = await fetchJson(chatEndpoint(normalizeBaseUrl(provider.baseUrl), dialect), {
    method: "POST",
    headers: chatHeaders(key, dialect),
    body: JSON.stringify(body),
  }, timeoutMs);
  return response;
}

async function findRecipe(provider, key, modelId) {
  if ((provider.dialect || "openai") === "anthropic") {
    const recipe = { tokenParam: "max_tokens", temperature: true };
    const response = await callChat(provider, key, modelId, "Reply with exactly: ok", 16, recipe);
    return response.ok ? { recipe, json: response.data } : null;
  }
  const results = await Promise.all(requestRecipes.map(async (recipe) => {
    const response = await callChat(provider, key, modelId, "Reply with exactly: ok", 16, recipe);
    return response.ok ? { recipe, json: response.data } : null;
  }));
  return results.find(Boolean) || null;
}

function usageObject(json) {
  return json?.usage || {};
}

function detectUsageShape(json) {
  const usage = usageObject(json);
  if (usage.input_tokens != null || usage.output_tokens != null) return "anthropic";
  if (usage.prompt_tokens != null || usage.completion_tokens != null) return "openai";
  return "unknown";
}

function reasoningSignal(json) {
  const usage = usageObject(json);
  const details = usage.completion_tokens_details || usage.output_tokens_details || {};
  const reasoningTokens = details.reasoning_tokens || usage.reasoning_tokens || details.thinking_tokens || 0;
  const message = json?.choices?.[0]?.message || {};
  const reasoningContent = message.reasoning_content || message.reasoning || "";
  const content = typeof message.content === "string"
    ? message.content
    : Array.isArray(json?.content)
      ? json.content.map((block) => block?.text || block?.thinking || "").join("")
      : "";
  const anthropicThinking = Array.isArray(json?.content) && json.content.some((block) => block?.type === "thinking" && String(block.thinking || "").length > 4);
  if (reasoningTokens > 0 || String(reasoningContent || "").trim().length > 4 || anthropicThinking) return true;
  if (/<think>[\s\S]{4,}?<\/think>|<thinking>[\s\S]{4,}?<\/thinking>/i.test(content)) return true;
  const completion = usage.completion_tokens || usage.output_tokens || 0;
  const visibleEstimate = Math.max(1, Math.ceil(String(content || "").length / 4));
  return completion > 50 && completion / visibleEstimate > 10;
}

function usageSample(json) {
  const usage = usageObject(json);
  const promptDetails = usage.prompt_tokens_details || usage.input_tokens_details || {};
  const completionDetails = usage.completion_tokens_details || usage.output_tokens_details || {};
  return {
    input: usage.prompt_tokens || usage.input_tokens || 0,
    output: Math.max(0, (usage.completion_tokens || usage.output_tokens || 0) - (completionDetails.reasoning_tokens || usage.reasoning_tokens || 0)),
    reasoning: completionDetails.reasoning_tokens || usage.reasoning_tokens || 0,
    cacheRead: Boolean(promptDetails.cached_tokens || usage.cache_read_input_tokens || usage.prompt_cache_hit_tokens),
    cacheWrite: Boolean(usage.cache_creation_input_tokens || promptDetails.cache_creation_tokens),
  };
}

function capabilityFallback(modelId, usable = true) {
  const isThink = /-(?:think|thinking|reasoner|reasoning)$/i.test(modelId) || /^o\d/i.test(modelId);
  return {
    usable,
    requestRecipe: { tokenParam: "max_tokens", temperature: true },
    acceptsTemperature: true,
    thinking: isThink,
    thinkingMethod: isThink ? thinkMethods[0] : null,
    thinkingLevels: isThink ? ["off", "on"] : [],
    cache: { read: false, write: false, method: "none" },
    streaming: { supported: false, reasoningDelta: "none" },
    contextWindow: null,
    usageSample: { input: 0, output: 0, reasoning: 0, cacheRead: false, cacheWrite: false },
    usageDialect: "unknown",
  };
}

function effortBodyForMethod(method, level) {
  const body = JSON.parse(JSON.stringify(method.body || {}));
  if (Object.prototype.hasOwnProperty.call(body, "reasoning_effort")) body.reasoning_effort = level;
  if (body.reasoning && typeof body.reasoning === "object" && Object.prototype.hasOwnProperty.call(body.reasoning, "effort")) {
    body.reasoning.effort = level;
  }
  return body;
}

/// Probes each candidate effort level against the live endpoint and keeps only
/// levels that both succeed and demonstrably produce reasoning output.
async function validateEffortLevels(provider, key, modelId, recipe, method, thinkingPrompt) {
  if (!method) return [];
  const dialect = provider.dialect || "openai";
  if (dialect === "anthropic" || method.id === "anthropic_thinking") {
    const entries = Object.entries(anthropicBudgetForLevel);
    const results = await Promise.all(entries.map(async ([level, budget]) => {
      const body = { thinking: { type: "enabled", budget_tokens: budget } };
      const response = await callChat(provider, key, modelId, thinkingPrompt, budget + 1024, recipe, body).catch(() => null);
      return response?.ok && reasoningSignal(response.data) ? level : null;
    }));
    const validated = results.filter(Boolean);
    return validated.length ? validated : method.levels || [];
  }
  const graded = (method.levels || []).some((level) => ["low", "medium", "high"].includes(level));
  if (!graded) return method.levels || [];
  const results = await Promise.all(effortLevelCandidates.map(async (level) => {
    const body = effortBodyForMethod(method, level);
    const response = await callChat(provider, key, modelId, thinkingPrompt, 768, recipe, body).catch(() => null);
    return response?.ok && reasoningSignal(response.data) ? level : null;
  }));
  const validated = results.filter(Boolean);
  return validated.length ? validated : method.levels || [];
}

// Stable filler well past typical 1024-token cache minimums, so an identical
// second request can hit the provider's prompt cache.
const cachePromptBlock = `Codex cache probe corpus. ${
  "The quick brown fox jumps over the lazy dog while meticulous auditors tally every cached token in the ledger. ".repeat(96)
}`;

/// Detects prompt-cache support by sending the same long prompt twice and
/// inspecting usage fields (OpenAI cached_tokens, DeepSeek prompt_cache_hit_tokens,
/// Anthropic cache_read/creation with explicit cache_control blocks).
async function detectCacheCapability(provider, key, modelId, recipe) {
  const result = { read: false, write: false, method: "none" };
  const dialect = provider.dialect || "openai";
  try {
    if (dialect === "anthropic") {
      const extra = { system: [{ type: "text", text: cachePromptBlock, cache_control: { type: "ephemeral" } }] };
      const first = await callChat(provider, key, modelId, "Reply with exactly: ok", 16, recipe, extra);
      if (!first?.ok) return result;
      const second = await callChat(provider, key, modelId, "Reply with exactly: ok", 16, recipe, extra);
      if (!second?.ok) return result;
      const firstUsage = usageObject(first.data);
      const secondUsage = usageObject(second.data);
      result.write = Boolean(firstUsage.cache_creation_input_tokens || secondUsage.cache_creation_input_tokens);
      result.read = Boolean(secondUsage.cache_read_input_tokens);
      if (result.read || result.write) result.method = "anthropic_explicit";
      return result;
    }
    const prompt = `${cachePromptBlock}\nReply with exactly: ok`;
    const first = await callChat(provider, key, modelId, prompt, 16, recipe);
    if (!first?.ok) return result;
    const second = await callChat(provider, key, modelId, prompt, 16, recipe);
    if (!second?.ok) return result;
    const firstSample = usageSample(first.data);
    const secondSample = usageSample(second.data);
    result.read = secondSample.cacheRead;
    result.write = firstSample.cacheWrite || secondSample.cacheWrite;
    if (result.read) result.method = "auto";
    return result;
  } catch {
    return result;
  }
}

/// Detects the model's real context window from provider metadata.
///
/// Sources, in order: the provider's /models entry (vLLM `max_model_len`,
/// OpenRouter `context_length`, misc gateways), then LiteLLM's /model/info
/// (`max_input_tokens`). Returns null when nothing trustworthy is advertised so
/// the registry keeps its existing value.
async function detectContextWindow(provider, key, modelId) {
  const base = normalizeBaseUrl(provider.baseUrl);
  const dialect = provider.dialect || "openai";
  const upstreamId = String(modelId);
  const contextFields = (row) => [
    row?.context_length,
    row?.max_model_len,
    row?.max_context_length,
    row?.context_window,
    row?.max_context_window,
    row?.max_input_tokens,
    row?.model_info?.max_input_tokens,
  ];
  const pick = (row) => {
    for (const value of contextFields(row)) {
      const n = Number(value);
      if (Number.isInteger(n) && n >= 1024) return n;
    }
    return null;
  };
  try {
    const request = modelListRequest(base, key, dialect);
    const response = await fetchJson(request.url, { headers: request.headers }, 9000);
    if (response.ok) {
      const rows = Array.isArray(response.data?.data) ? response.data.data : Array.isArray(response.data?.models) ? response.data.models : [];
      const row = rows.find((r) => (r?.id || r?.slug || r?.name || r?.model) === upstreamId);
      const found = row ? pick(row) : null;
      if (found) return found;
    }
  } catch {}
  if (dialect !== "anthropic") {
    for (const infoUrl of [`${base}/model/info`, `${base.replace(/\/v\d+$/i, "")}/model/info`]) {
      try {
        const response = await fetchJson(infoUrl, { headers: { Authorization: `Bearer ${key}` } }, 9000);
        if (!response.ok) continue;
        const rows = Array.isArray(response.data?.data) ? response.data.data : [];
        const row = rows.find((r) => r?.model_name === upstreamId || r?.model_info?.key === upstreamId);
        const found = row ? pick(row) : null;
        if (found) return found;
      } catch {}
    }
  }
  return null;
}

/// Detects SSE streaming support and which field carries reasoning deltas
/// (reasoning_content, reasoning, or <think> tags inline in content).
async function detectStreamingCapability(provider, key, modelId, recipe, thinkingBody) {
  const streaming = { supported: false, reasoningDelta: "none" };
  const dialect = provider.dialect || "openai";
  if (dialect === "anthropic") {
    // The Messages API always streams via content_block_delta; thinking arrives
    // as thinking_delta blocks. No probe needed.
    streaming.supported = true;
    streaming.reasoningDelta = "anthropic_thinking_delta";
    return streaming;
  }
  const prompt = "A bat and a ball cost $1.10 total. The bat costs $1 more than the ball. What does the ball cost? Reason briefly, then answer.";
  const attempt = async (withUsageOption) => {
    const body = buildProbeBody("openai", modelId, prompt, 256, recipe, thinkingBody || {});
    body.stream = true;
    if (withUsageOption) body.stream_options = { include_usage: true };
    return fetchJson(chatEndpoint(normalizeBaseUrl(provider.baseUrl), "openai"), {
      method: "POST",
      headers: chatHeaders(key, "openai"),
      body: JSON.stringify(body),
    }, 30000);
  };
  try {
    let response = await attempt(true);
    if (!response.ok) response = await attempt(false);
    const text = response.text || "";
    if (!response.ok || !/data:/.test(text)) return streaming;
    streaming.supported = /"delta"/.test(text);
    if (/"reasoning_content"\s*:\s*"[^"]/.test(text)) streaming.reasoningDelta = "reasoning_content";
    else if (/"reasoning"\s*:\s*"[^"]/.test(text)) streaming.reasoningDelta = "reasoning";
    else if (/<think>/.test(text) || /<\|channel>/.test(text)) streaming.reasoningDelta = "content_think";
    return streaming;
  } catch {
    return streaming;
  }
}

// Emit a live probe-progress line on stderr. The TUI streams stderr into the
// probe spinner so the user can watch each step; stdout stays clean for the
// human-readable summary and the JSON catalog reads.
function probeProgress(msg) {
  try {
    process.stderr.write(`${msg}\n`);
  } catch {}
}

async function detectModelCapability(provider, key, modelId) {
  probeProgress(`▸ ${modelId}: detecting request format…`);
  const found = await findRecipe(provider, key, modelId);
  if (!found) {
    probeProgress(`✗ ${modelId}: no working request format`);
    return capabilityFallback(modelId, false);
  }
  const capability = capabilityFallback(modelId, true);
  capability.requestRecipe = found.recipe;
  capability.acceptsTemperature = Boolean(found.recipe.temperature);
  capability.usageDialect = detectUsageShape(found.json);
  capability.usageSample = usageSample(found.json);

  probeProgress(`▸ ${modelId}: reasoning / effort levels…`);
  const thinkingPrompt = "A bat and a ball cost $1.10 total. The bat costs $1 more than the ball. What does the ball cost? Reason briefly, then answer.";
  const base = await callChat(provider, key, modelId, thinkingPrompt, 256, found.recipe).catch(() => null);
  if (base?.ok && reasoningSignal(base.data)) {
    capability.thinking = true;
    capability.thinkingMethod = { id: "intrinsic", body: {}, levels: ["on"], dialect: "any" };
    capability.thinkingLevels = ["on"];
    capability.usageSample = usageSample(base.data);
    const disableResults = await Promise.all(disableMethods.map(async (method) => {
      const response = await callChat(provider, key, modelId, thinkingPrompt, 256, found.recipe, method.body).catch(() => null);
      return { method, silenced: Boolean(response?.ok && !reasoningSignal(response.data)) };
    }));
    const off = disableResults.find((result) => result.silenced);
    if (off) {
      capability.thinkingMethod.offBody = off.method.body;
      capability.thinkingLevels = ["off", "on"];
    }
    // Intrinsic thinkers (OpenAI o-series, gpt-5*) often also accept graded
    // reasoning_effort. If two or more levels validate, upgrade from a binary
    // on/off to real effort levels.
    if ((provider.dialect || "openai") !== "anthropic") {
      const effortMethod = thinkMethods.find((method) => method.id === "reasoning_effort");
      const validated = await validateEffortLevels(provider, key, modelId, found.recipe, effortMethod, thinkingPrompt);
      const gradedLevels = validated.filter((level) => effortLevelCandidates.includes(level));
      if (gradedLevels.length >= 2) {
        capability.thinkingMethod = { ...effortMethod, levels: gradedLevels, offBody: off ? off.method.body : effortMethod.offBody };
        capability.thinkingLevels = off ? ["off", ...gradedLevels] : gradedLevels;
      }
    }
  } else {
    const candidates = thinkMethods.filter((method) => method.dialect === "any" || method.dialect === (provider.dialect || "openai"));
    const results = await Promise.all(candidates.map(async (method) => {
      const response = await callChat(provider, key, modelId, thinkingPrompt, 512, found.recipe, method.body).catch(() => null);
      return { method, response };
    }));
    const hit = results.find((result) => result.response?.ok && reasoningSignal(result.response.data));
    if (hit) {
      capability.thinking = true;
      capability.usageSample = usageSample(hit.response.data);
      const validated = await validateEffortLevels(provider, key, modelId, found.recipe, hit.method, thinkingPrompt);
      capability.thinkingMethod = { ...hit.method, levels: validated };
      capability.thinkingLevels = ["off", ...validated];
    }
  }

  probeProgress(`▸ ${modelId}: context window…`);
  capability.contextWindow = await detectContextWindow(provider, key, modelId);
  probeProgress(`▸ ${modelId}: prompt cache…`);
  capability.cache = await detectCacheCapability(provider, key, modelId, found.recipe);
  probeProgress(`▸ ${modelId}: streaming…`);
  capability.streaming = await detectStreamingCapability(
    provider,
    key,
    modelId,
    found.recipe,
    capability.thinking && capability.thinkingMethod?.body ? capability.thinkingMethod.body : {},
  );
  const thinkDesc = capability.thinking
    ? `thinking [${(capability.thinkingLevels || []).join("/") || "on"}]`
    : "no-thinking";
  probeProgress(
    `✓ ${modelId}: ${thinkDesc}, cache ${capability.cache ? "yes" : "no"}, streaming ${capability.streaming ? "yes" : "no"}`,
  );
  return capability;
}

function applyCapability(config, capability) {
  const selectedThinking = config.thinking || (/(?:^|[-_])(?:think|thinking|reasoner|reasoning)$/i.test(config.upstreamModel || "") ? "on" : "off");
  let thinkingMethod = capability.thinkingMethod;
  if (
    capability.thinking &&
    thinkingMethod?.id === "intrinsic" &&
    defaultRequestBody(config.upstreamModel || "").chat_template_kwargs?.enable_thinking === true
  ) {
    thinkingMethod = thinkMethods[0];
  }

  config.capabilities = {
    usable: capability.usable,
    thinking: capability.thinking,
    acceptsTemperature: capability.acceptsTemperature,
    cache: capability.cache,
    streaming: capability.streaming || { supported: false, reasoningDelta: "none" },
    usageDialect: capability.usageDialect,
    usageSample: capability.usageSample,
  };
  config.requestRecipe = capability.requestRecipe;
  config.thinking = selectedThinking;
  config.thinkingMethod = thinkingMethod;
  config.thinkingLevels = capability.thinkingLevels;
  if (Number.isInteger(capability.contextWindow) && capability.contextWindow >= 1024) {
    config.contextWindow = capability.contextWindow;
    config.maxContextWindow = capability.contextWindow;
  }
  return config;
}

function defaultModelConfig(providerId, modelId, provider) {
  const contextWindow = Number(provider?.defaultContextWindow || 98304);
  const isThink = /-(?:think|thinking)$/i.test(modelId);
  const isFast = /fast/i.test(modelId);
  const isSmart = /smart/i.test(modelId);
  const config = {
    provider: providerId,
    upstreamModel: modelId,
    enabled: false,
    contextWindow: isSmart ? 262144 : contextWindow,
    maxContextWindow: isSmart ? 262144 : contextWindow,
    effectiveContextWindowPercent: isFast ? 83 : 95,
    outputTokenLimit: isFast ? 16384 : null,
    thinking: isThink ? "on" : "off",
    thinkingMethod: isThink ? thinkMethods[0] : null,
    thinkingLevels: isThink ? ["off", "on"] : [],
    requestRecipe: { tokenParam: "max_tokens", temperature: true },
    capabilities: {
      usable: true,
      thinking: isThink,
      acceptsTemperature: true,
      cache: { read: false, write: false, method: "none" },
      usageDialect: "unknown",
      usageSample: { input: 0, output: 0, reasoning: 0, cacheRead: false, cacheWrite: false },
    },
    normalizeSystem: modelId === "Chitti-Fast-Think" ? "ascii" : "off",
  };
  return config;
}

function addModelOrder(registry, modelId) {
  if (!registry.modelOrder.includes(modelId)) registry.modelOrder.push(modelId);
}

function activeProviderId(registry, modelConfig) {
  return modelConfig?.provider || registry.defaultProvider || "";
}

function firstModelId(registry, includeDisabled = false) {
  return orderedModels(registry, includeDisabled)[0]?.[0] || "";
}

function removeModelIds(registry, modelIds) {
  const uniqueIds = Array.from(new Set(modelIds));
  const removed = [];
  const missing = [];
  for (const modelId of uniqueIds) {
    if (registry.models[modelId]) {
      delete registry.models[modelId];
      removed.push(modelId);
    } else {
      missing.push(modelId);
    }
  }
  const removedSet = new Set(removed);
  registry.modelOrder = (registry.modelOrder || []).filter((modelId) => !removedSet.has(modelId));
  if (removedSet.has(registry.defaultModel)) {
    registry.defaultModel = firstModelId(registry, false) || firstModelId(registry, true) || "";
  }
  return { removed, missing };
}

function splitNames(values) {
  return values.flatMap((value) => String(value).split(",")).map((value) => value.trim()).filter(Boolean);
}

function toInt(value, label) {
  const number = Number(value);
  if (!Number.isInteger(number) || number < 0) throw new Error(`${label} must be a non-negative integer`);
  return number;
}

function orderedModels(registry, includeDisabled = false) {
  const seen = new Set();
  const ordered = [];
  for (const modelId of registry.modelOrder || []) {
    if (!registry.models[modelId] || seen.has(modelId)) continue;
    seen.add(modelId);
    if (includeDisabled || registry.models[modelId].enabled) ordered.push([modelId, registry.models[modelId]]);
  }
  for (const [modelId, config] of Object.entries(registry.models || {})) {
    if (seen.has(modelId)) continue;
    if (includeDisabled || config.enabled) ordered.push([modelId, config]);
  }
  return ordered;
}

function selectTemplate(existingCatalog, modelId) {
  const exact = existingCatalog.models.find((model) => model.slug === modelId);
  if (exact) return exact;
  if (existingCatalog.models[0]) return existingCatalog.models[0];
  throw new Error(`${catalogPath} has no template models`);
}

function reasoningEffortForThinkingLevel(level) {
  const value = String(level || "").toLowerCase();
  if (value === "off") return "none";
  if (value === "mid") return "medium";
  return value || "none";
}

function reasoningDescription(effort) {
  switch (effort) {
    case "auto":
      return "Use the detected provider default for this model";
    case "none":
      return "Disable provider thinking";
    case "on":
      return "Enable provider thinking";
    case "low":
      return "Use low provider reasoning effort";
    case "medium":
      return "Use medium provider reasoning effort";
    case "high":
      return "Use high provider reasoning effort";
    case "xhigh":
      return "Use extra high provider reasoning effort";
    default:
      return `Use provider reasoning level ${effort}`;
  }
}

function catalogReasoningEfforts(config) {
  if (!config.capabilities?.thinking) return ["none"];
  const efforts = ["auto"];
  for (const level of config.thinkingLevels || []) {
    const effort = reasoningEffortForThinkingLevel(level);
    if (effort && !efforts.includes(effort)) efforts.push(effort);
  }
  if (efforts.length === 1) efforts.push("on");
  return efforts;
}

function catalogDefaultReasoning(config, efforts) {
  const selected = config.thinking === "off" ? "none" : reasoningEffortForThinkingLevel(config.thinking || "auto");
  if (efforts.includes(selected)) return selected;
  if (efforts.includes("auto")) return "auto";
  return efforts[0] || "none";
}

function buildModelEntry(template, modelId, config, registry) {
  const provider = registry.providers?.[config.provider] || {};
  const entry = JSON.parse(JSON.stringify(template));
  entry.slug = modelId;
  entry.display_name = config.displayName || modelId;
  entry.description = config.description || `${provider.name || config.provider || "Custom"} model ${modelId}`;
  if (Number.isInteger(config.contextWindow)) entry.context_window = config.contextWindow;
  if (Number.isInteger(config.maxContextWindow)) entry.max_context_window = config.maxContextWindow;
  if (Number.isInteger(config.effectiveContextWindowPercent)) {
    entry.effective_context_window_percent = config.effectiveContextWindowPercent;
  }
  const reasoningEfforts = catalogReasoningEfforts(config);
  entry.default_reasoning_level = catalogDefaultReasoning(config, reasoningEfforts);
  entry.supported_reasoning_levels = reasoningEfforts.map((effort) => ({
    effort,
    description: reasoningDescription(effort),
  }));
  // Expose MCP tools directly (browser, etc.) instead of deferring them behind
  // tool-search, which weaker models cannot drive. See mcp_tool_exposure.rs.
  entry.supports_search_tool = false;
  return entry;
}

function buildCatalog(registry, { backup = true } = {}) {
  const existingCatalog = fs.existsSync(catalogPath) ? readJson(catalogPath) : { models: [] };
  if (!Array.isArray(existingCatalog.models)) throw new Error(`${catalogPath} must contain a models array`);
  const models = orderedModels(registry, false).map(([modelId, config]) => {
    const template = selectTemplate(existingCatalog, modelId);
    return buildModelEntry(template, modelId, config, registry);
  });
  const nextCatalog = { ...existingCatalog, models };
  const nextText = `${JSON.stringify(nextCatalog, null, 2)}\n`;
  const currentText = fs.existsSync(catalogPath) ? fs.readFileSync(catalogPath, "utf8") : "";
  if (nextText === currentText) return { changed: false, backup: "" };
  const backupPath = backup ? backupFile(catalogPath) : "";
  writeJsonAtomic(catalogPath, nextCatalog, fileMode(catalogPath));
  return { changed: true, backup: backupPath };
}

function saveRegistryAndCatalog(registry) {
  saveRegistry(registry);
  return buildCatalog(registry);
}

async function fetchModels(registry, providerId) {
  const provider = requireProvider(registry, providerId);
  const key = providerKey(provider);
  if (!key) throw new Error(`No API key configured for provider ${providerId}`);
  const probe = await probeProviderEndpoint(provider.baseUrl, key);
  provider.baseUrl = probe.resolved;
  provider.providerKind = probe.providerKind;
  provider.authStyle = probe.authStyle;
  provider.dialect = probe.dialect;
  return probe.models;
}

async function readStdin() {
  const chunks = [];
  for await (const chunk of process.stdin) chunks.push(Buffer.from(chunk));
  return Buffer.concat(chunks).toString("utf8");
}

function printProviderList(registry, json) {
  const rows = Object.entries(registry.providers || {}).map(([id, provider]) => ({
    id,
    name: provider.name || id,
    baseUrl: provider.baseUrl || "",
    wireApi: provider.wireApi || "chat_completions",
    kind: provider.providerKind || "unknown",
    dialect: provider.dialect || "openai",
    key: keyStatus(provider),
  }));
  if (json) {
    console.log(JSON.stringify(rows, null, 2));
    return;
  }
  for (const row of rows) {
    console.log(`${row.id}\t${row.baseUrl}\tkind=${row.kind}\tdialect=${row.dialect}\tkey=${row.key}`);
  }
}

function printModelList(registry, { all = false, json = false } = {}) {
  const rows = orderedModels(registry, all).map(([id, config]) => ({
    id,
    enabled: Boolean(config.enabled),
    provider: config.provider || registry.defaultProvider,
    upstreamModel: config.upstreamModel || id,
    context: config.contextWindow ?? null,
    output: config.outputTokenLimit ?? "default",
    thinking: config.thinking || "auto",
    thinkingLevels: config.thinkingLevels || [],
    thinkingMethod: config.thinkingMethod?.id || null,
    capThinking: Boolean(config.capabilities?.thinking),
    cache: config.capabilities?.cache || { read: false, write: false, method: "none" },
    streaming: config.capabilities?.streaming || { supported: false, reasoningDelta: "none" },
    usable: config.capabilities?.usable ?? true,
    recipe: config.requestRecipe || null,
    normalizeSystem: config.normalizeSystem || "off",
  }));
  if (json) {
    console.log(JSON.stringify(rows, null, 2));
    return;
  }
  for (const row of rows) {
    console.log(`${row.enabled ? "on " : "off"}\t${row.id}\tprovider=${row.provider}\tcontext=${row.context}\toutput=${row.output}\tselected_thinking=${row.thinking}\tcap_thinking=${row.capThinking}\tlevels=${row.thinkingLevels.join("/") || "none"}\tmethod=${row.thinkingMethod || "none"}\tcache=${row.cache.read ? "read" : "none"}\tstream=${row.streaming.supported ? row.streaming.reasoningDelta : "no"}\tusable=${row.usable}\tnormalize=${row.normalizeSystem}`);
  }
}

async function commandProvider(registry, subcommand, args) {
  if (subcommand === "list") {
    const opts = parseArgs(args);
    printProviderList(registry, Boolean(opts.json));
    return false;
  }
  if (subcommand === "connect") {
    const providerId = args[0];
    if (!providerId) throw new Error("provider connect requires an id");
    const opts = parseArgs(args.slice(1));
    if (!opts["base-url"]) throw new Error("provider connect requires --base-url");

    let key = "";
    let authPatch = {};
    if (opts.stdin) {
      key = (await readStdin()).trim();
      if (!key) throw new Error("stdin did not contain an API key");
      const existing = registry.providers?.[providerId] || {};
      const keyFile = expandHome(existing.apiKeyFile || (providerId === "lumenatech" ? defaultKeyPath : path.join(codexosHome, "keys", `${providerId}.key`)));
      fs.mkdirSync(path.dirname(keyFile), { recursive: true, mode: 0o700 });
      fs.writeFileSync(keyFile, `${key}\n`, { mode: 0o600 });
      fs.chmodSync(keyFile, 0o600);
      authPatch = { apiKeyFile: keyFile.replace(homeDir, "~") };
    } else if (opts["api-key-file"]) {
      authPatch = { apiKeyFile: opts["api-key-file"] };
      key = fs.readFileSync(expandHome(opts["api-key-file"]), "utf8").trim();
    } else if (opts["env-key"]) {
      authPatch = { envKey: opts["env-key"] };
      key = String(process.env[opts["env-key"]] || "").trim();
      if (!key) throw new Error(`environment variable ${opts["env-key"]} is empty`);
    } else {
      throw new Error("provider connect requires --stdin, --api-key-file, or --env-key");
    }

    const probe = await probeProviderEndpoint(opts["base-url"], key);
    const provider = {
      ...(registry.providers[providerId] || {}),
      ...authPatch,
      name: opts.name || registry.providers[providerId]?.name || providerId,
      baseUrl: probe.resolved,
      rawBaseUrl: opts["base-url"],
      wireApi: opts["wire-api"] || registry.providers[providerId]?.wireApi || "chat_completions",
      providerKind: probe.providerKind,
      authStyle: probe.authStyle,
      dialect: probe.dialect,
      defaultContextWindow: opts["default-context"] ? toInt(opts["default-context"], "--default-context") : registry.providers[providerId]?.defaultContextWindow || 98304,
    };
    if (authPatch.apiKeyFile) delete provider.envKey;
    if (authPatch.envKey) delete provider.apiKeyFile;
    registry.providers[providerId] = provider;
    registry.defaultProvider ||= providerId;

    let added = 0;
    for (const modelId of probe.models) {
      if (!registry.models[modelId]) {
        registry.models[modelId] = defaultModelConfig(providerId, modelId, provider);
        added++;
      } else {
        registry.models[modelId].provider = providerId;
        registry.models[modelId].upstreamModel ||= modelId;
      }
      if (opts.enable) registry.models[modelId].enabled = true;
      addModelOrder(registry, modelId);
    }

    if (opts.probe) {
      const limit = opts["probe-limit"] ? toInt(opts["probe-limit"], "--probe-limit") : probe.models.length;
      for (const modelId of probe.models.slice(0, limit)) {
        const capability = await detectModelCapability(provider, key, modelId);
        applyCapability(registry.models[modelId], capability);
      }
    }

    const result = saveRegistryAndCatalog(registry);
    console.log(`connected provider: ${providerId}`);
    console.log(`resolved: ${probe.resolved}`);
    console.log(`models: ${probe.models.length}; added: ${added}; probed: ${opts.probe ? Math.min(probe.models.length, opts["probe-limit"] ? toInt(opts["probe-limit"], "--probe-limit") : probe.models.length) : 0}`);
    console.log(`catalog ${result.changed ? "rebuilt" : "unchanged"}`);
    return false;
  }
  if (subcommand === "add") {
    const providerId = args[0];
    if (!providerId) throw new Error("provider add requires an id");
    const opts = parseArgs(args.slice(1));
    if (!opts["base-url"]) throw new Error("provider add requires --base-url");
    registry.providers[providerId] = {
      ...(registry.providers[providerId] || {}),
      name: opts.name || registry.providers[providerId]?.name || providerId,
      baseUrl: normalizeBaseUrl(opts["base-url"]),
      wireApi: opts["wire-api"] || registry.providers[providerId]?.wireApi || "chat_completions",
      defaultContextWindow: opts["default-context"] ? toInt(opts["default-context"], "--default-context") : registry.providers[providerId]?.defaultContextWindow || 98304,
    };
    if (opts["api-key-file"]) registry.providers[providerId].apiKeyFile = opts["api-key-file"];
    if (opts["env-key"]) registry.providers[providerId].envKey = opts["env-key"];
    saveRegistry(registry);
    console.log(`provider saved: ${providerId}`);
    return false;
  }
  if (subcommand === "remove") {
    const providerId = args[0];
    if (!providerId) throw new Error("provider remove requires an id");
    if (args.length > 1) throw new Error("provider remove accepts exactly one provider id");
    requireProvider(registry, providerId);

    const modelIds = Object.entries(registry.models || {})
      .filter(([, config]) => activeProviderId(registry, config) === providerId)
      .map(([modelId]) => modelId);
    const { removed } = removeModelIds(registry, modelIds);
    delete registry.providers[providerId];
    if (registry.defaultProvider === providerId) {
      registry.defaultProvider = Object.keys(registry.providers || {})[0] || "";
    }
    const result = saveRegistryAndCatalog(registry);
    console.log(`removed provider: ${providerId}`);
    console.log(`removed model(s): ${removed.join(", ") || "(none)"}`);
    console.log(`catalog ${result.changed ? "rebuilt" : "unchanged"}`);
    return false;
  }
  throw new Error(`Unknown provider command: ${subcommand || ""}`);
}

async function commandAuth(registry, subcommand, args) {
  if (subcommand !== "set") throw new Error(`Unknown auth command: ${subcommand || ""}`);
  const providerId = args[0];
  if (!providerId) throw new Error("auth set requires a provider id");
  const provider = requireProvider(registry, providerId);
  const opts = parseArgs(args.slice(1));
  if (opts["env-key"]) {
    provider.envKey = opts["env-key"];
    delete provider.apiKeyFile;
    saveRegistry(registry);
    console.log(`auth updated for ${providerId}: env:${opts["env-key"]}`);
    return false;
  }
  if (opts["api-key-file"]) {
    provider.apiKeyFile = opts["api-key-file"];
    delete provider.envKey;
    saveRegistry(registry);
    console.log(`auth updated for ${providerId}: file:${opts["api-key-file"]}`);
    return false;
  }
  if (opts.stdin) {
    const key = (await readStdin()).trim();
    if (!key) throw new Error("stdin did not contain an API key");
    const keyFile = expandHome(provider.apiKeyFile || (providerId === "lumenatech" ? defaultKeyPath : path.join(codexosHome, "keys", `${providerId}.key`)));
    fs.mkdirSync(path.dirname(keyFile), { recursive: true, mode: 0o700 });
    fs.writeFileSync(keyFile, `${key}\n`, { mode: 0o600 });
    fs.chmodSync(keyFile, 0o600);
    provider.apiKeyFile = keyFile.replace(homeDir, "~");
    delete provider.envKey;
    saveRegistry(registry);
    console.log(`auth updated for ${providerId}: file:${provider.apiKeyFile}`);
    return false;
  }
  throw new Error("auth set requires --stdin, --api-key-file, or --env-key");
}

async function commandModels(registry, subcommand, args) {
  if (subcommand === "list") {
    const opts = parseArgs(args);
    printModelList(registry, { all: Boolean(opts.all), json: Boolean(opts.json) });
    return false;
  }

  if (subcommand === "refresh") {
    const opts = parseArgs(args);
    const providerId = opts.provider || registry.defaultProvider;
    const provider = requireProvider(registry, providerId);
    const key = providerKey(provider);
    if (!key) throw new Error(`No API key configured for provider ${providerId}`);
    const modelIds = await fetchModels(registry, providerId);
    let added = 0;
    for (const modelId of modelIds) {
      if (!registry.models[modelId]) {
        registry.models[modelId] = defaultModelConfig(providerId, modelId, provider);
        added++;
      }
      if (opts.enable) registry.models[modelId].enabled = true;
      addModelOrder(registry, modelId);
    }
    let probed = 0;
    if (opts.probe) {
      const limit = opts["probe-limit"] ? toInt(opts["probe-limit"], "--probe-limit") : modelIds.length;
      for (const modelId of modelIds.slice(0, limit)) {
        const capability = await detectModelCapability(provider, key, modelId);
        applyCapability(registry.models[modelId], capability);
        probed++;
      }
    }
    const result = saveRegistryAndCatalog(registry);
    console.log(`refreshed ${modelIds.length} model(s) from ${providerId}; added ${added}`);
    if (opts.probe) console.log(`probed ${probed} model(s)`);
    console.log(`catalog ${result.changed ? "rebuilt" : "unchanged"}`);
    return false;
  }

  if (subcommand === "probe") {
    const opts = parseArgs(args);
    const modelIds = splitNames(opts._);
    if (!modelIds.length) throw new Error("models probe requires at least one model");
    for (const modelId of modelIds) {
      // Re-probes must hit the model's own provider, not the registry default —
      // --provider stays available as an explicit override.
      const providerId = opts.provider || registry.models[modelId]?.provider || registry.defaultProvider;
      const provider = requireProvider(registry, providerId);
      const key = providerKey(provider);
      if (!key) throw new Error(`No API key configured for provider ${providerId}`);
      if (!registry.models[modelId]) registry.models[modelId] = defaultModelConfig(providerId, modelId, provider);
      const capability = await detectModelCapability(provider, key, modelId);
      applyCapability(registry.models[modelId], capability);
      addModelOrder(registry, modelId);
      const saved = registry.models[modelId];
      const cache = capability.cache || {};
      const streaming = capability.streaming || {};
      console.log(`${modelId}: provider=${providerId} usable=${capability.usable} cap_thinking=${capability.thinking} levels=${(saved.thinkingLevels || []).join("/") || "none"} selected_thinking=${saved.thinking} method=${saved.thinkingMethod?.id || "none"} context=${saved.contextWindow ?? "unknown"}${capability.contextWindow ? " (probed)" : ""} temperature=${capability.acceptsTemperature} cache_read=${Boolean(cache.read)} cache_write=${Boolean(cache.write)} streaming=${Boolean(streaming.supported)} reasoning_delta=${streaming.reasoningDelta || "none"}`);
    }
    const result = saveRegistryAndCatalog(registry);
    console.log(`catalog ${result.changed ? "rebuilt" : "unchanged"}`);
    return false;
  }

  if (subcommand === "enable" || subcommand === "disable") {
    const modelIds = splitNames(args);
    if (!modelIds.length) throw new Error(`models ${subcommand} requires at least one model`);
    for (const modelId of modelIds) {
      if (!registry.models[modelId]) {
        const providerId = registry.defaultProvider;
        registry.models[modelId] = defaultModelConfig(providerId, modelId, registry.providers[providerId]);
      }
      registry.models[modelId].enabled = subcommand === "enable";
      addModelOrder(registry, modelId);
    }
    const result = saveRegistryAndCatalog(registry);
    console.log(`${subcommand}d: ${modelIds.join(", ")}`);
    console.log(`catalog ${result.changed ? "rebuilt" : "unchanged"}`);
    return false;
  }

  if (subcommand === "remove") {
    const opts = parseArgs(args);
    const modelIds = splitNames(opts._);
    if (!modelIds.length) throw new Error("models remove requires at least one model");
    if (opts.provider) requireProvider(registry, opts.provider);
    const providerMismatches = [];
    const targets = [];
    const missing = [];
    for (const modelId of modelIds) {
      const config = registry.models[modelId];
      if (!config) {
        missing.push(modelId);
        continue;
      }
      if (opts.provider && activeProviderId(registry, config) !== opts.provider) {
        providerMismatches.push(modelId);
        continue;
      }
      targets.push(modelId);
    }
    const removed = removeModelIds(registry, targets).removed;
    const result = saveRegistryAndCatalog(registry);
    console.log(`removed model(s): ${removed.join(", ") || "(none)"}`);
    if (missing.length) console.log(`missing model(s): ${missing.join(", ")}`);
    if (providerMismatches.length) {
      console.log(`provider mismatch model(s): ${providerMismatches.join(", ")}`);
    }
    console.log(`catalog ${result.changed ? "rebuilt" : "unchanged"}`);
    return false;
  }

  if (subcommand === "set") {
    const modelId = args[0];
    if (!modelId) throw new Error("models set requires a model id");
    const opts = parseArgs(args.slice(1));
    if (!registry.models[modelId]) {
      const providerId = opts.provider || registry.defaultProvider;
      registry.models[modelId] = defaultModelConfig(providerId, modelId, registry.providers[providerId]);
    }
    const config = registry.models[modelId];
    if (opts.provider) {
      requireProvider(registry, opts.provider);
      config.provider = opts.provider;
    }
    if (opts["upstream-model"]) config.upstreamModel = opts["upstream-model"];
    if (opts["display-name"]) config.displayName = opts["display-name"];
    if (opts.context) {
      config.contextWindow = toInt(opts.context, "--context");
      config.maxContextWindow = config.contextWindow;
      // An explicitly asserted window is authoritative: honor it literally instead
      // of applying the safety haircut meant for windows we could not probe.
      // Without this, asking for 131072 would still surface as ~124k (95%).
      // --effective-percent below can still override.
      config.effectiveContextWindowPercent = 100;
    }
    if (opts["effective-percent"] !== undefined) {
      const percent = toInt(opts["effective-percent"], "--effective-percent");
      if (percent < 1 || percent > 100) throw new Error("--effective-percent must be between 1 and 100");
      config.effectiveContextWindowPercent = percent;
    }
    if (opts.output !== undefined) {
      const value = String(opts.output).toLowerCase();
      if (value === "none" || value === "default" || value === "null") config.outputTokenLimit = null;
      else config.outputTokenLimit = toInt(opts.output, "--output");
    }
    if (opts.thinking) {
      const value = String(opts.thinking).toLowerCase();
      const allowed = new Set(["on", "off", "auto", "low", "medium", "high", ...(config.thinkingLevels || []).map((level) => String(level).toLowerCase())]);
      if (!allowed.has(value)) throw new Error(`--thinking must be one of: ${Array.from(allowed).join(", ")}`);
      config.thinking = value;
    }
    if (opts["normalize-system"]) {
      const value = String(opts["normalize-system"]).toLowerCase();
      if (!["ascii", "off"].includes(value)) throw new Error("--normalize-system must be ascii or off");
      config.normalizeSystem = value;
    }
    addModelOrder(registry, modelId);
    const result = saveRegistryAndCatalog(registry);
    console.log(`model updated: ${modelId}`);
    console.log(`catalog ${result.changed ? "rebuilt" : "unchanged"}`);
    return false;
  }

  throw new Error(`Unknown models command: ${subcommand || ""}`);
}

function commandCatalog(registry, subcommand) {
  if (subcommand !== "build") throw new Error(`Unknown catalog command: ${subcommand || ""}`);
  const result = buildCatalog(registry);
  console.log(`catalog ${result.changed ? "rebuilt" : "unchanged"}: ${catalogPath}`);
  if (result.backup) console.log(`backup: ${result.backup}`);
  return false;
}

async function commandDoctor(registry, args) {
  const opts = parseArgs(args);
  const catalog = fs.existsSync(catalogPath) ? readJson(catalogPath) : { models: [] };
  const catalogSlugs = new Set((catalog.models || []).map((model) => model.slug));
  const enabled = orderedModels(registry, false).map(([id]) => id);
  const missing = enabled.filter((id) => !catalogSlugs.has(id));
  const providers = Object.entries(registry.providers || {}).map(([id, provider]) => `${id}:${keyStatus(provider)}`);
  console.log(`registry: ${registryPath}`);
  console.log(`catalog: ${catalogPath}`);
  console.log(`enabled models: ${enabled.join(", ") || "(none)"}`);
  console.log(`providers: ${providers.join(", ") || "(none)"}`);
  if (missing.length) throw new Error(`catalog missing enabled model(s): ${missing.join(", ")}`);
  if (opts.online) {
    for (const providerId of Object.keys(registry.providers || {})) {
      const models = await fetchModels(registry, providerId);
      console.log(`${providerId} online models: ${models.length}`);
    }
  }
  console.log("doctor ok");
  return false;
}

async function main() {
  const [command, subcommand, ...args] = process.argv.slice(2);
  if (!command || command === "help" || command === "--help" || command === "-h") {
    usage();
    return;
  }
  const registry = loadRegistry();
  if (command === "doctor") {
    await commandDoctor(registry, [subcommand, ...args].filter(Boolean));
    return;
  }
  if (command === "registry" && subcommand === "show") {
    const opts = parseArgs(args);
    const output = redactRegistry(registry);
    console.log(opts.json ? JSON.stringify(output, null, 2) : JSON.stringify(output, null, 2));
    return;
  }
  if (command === "provider") {
    await commandProvider(registry, subcommand, args);
    return;
  }
  if (command === "auth") {
    await commandAuth(registry, subcommand, args);
    return;
  }
  if (command === "models") {
    await commandModels(registry, subcommand, args);
    return;
  }
  if (command === "catalog") {
    commandCatalog(registry, subcommand);
    return;
  }
  throw new Error(`Unknown command: ${command}`);
}

main().catch((error) => {
  console.error(`retrace-admin: ${error.message || String(error)}`);
  process.exit(1);
});
