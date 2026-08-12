import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const proxyPath = fileURLToPath(new URL("./responses-chat-proxy.mjs", import.meta.url));

function listen(server) {
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolve(server.address().port));
  });
}

function close(server) {
  return new Promise((resolve, reject) => {
    server.close((error) => error ? reject(error) : resolve());
  });
}

async function waitForFile(file, child) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (child.exitCode !== null) throw new Error(`proxy exited early with ${child.exitCode}`);
    try {
      return (await readFile(file, "utf8")).trim();
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error("proxy readiness timeout");
}

function sendChatStream(res, chunks) {
  res.writeHead(200, { "content-type": "text/event-stream" });
  for (const chunk of chunks) res.write(`data: ${JSON.stringify(chunk)}\n\n`);
  res.end("data: [DONE]\n\n");
}

test("transaction guard filters tools and repairs truncated arguments", async () => {
  const tempDir = await mkdtemp(path.join(os.tmpdir(), "retrace-proxy-test-"));
  const readyFile = path.join(tempDir, "ready");
  const apiKeyFile = path.join(tempDir, "api_key");
  const modelCatalogFile = path.join(tempDir, "models.json");
  const registryFile = path.join(tempDir, "registry.json");
  const upstreamBodies = [];

  const upstream = http.createServer((req, res) => {
    const chunks = [];
    req.on("data", (chunk) => chunks.push(chunk));
    req.on("end", () => {
      const body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
      upstreamBodies.push(body);
      if (upstreamBodies.length === 1) {
        sendChatStream(res, [{
          choices: [{ delta: {
            content: "complete draft",
            tool_calls: [{
              index: 0,
              id: "call_blocked",
              function: { name: "unrelated_tool", arguments: "{\"x\":1}" },
            }],
          } }],
          usage: { prompt_tokens: 10, completion_tokens: 5 },
        }]);
        return;
      }
      sendChatStream(res, [{
        choices: [{ delta: {
          tool_calls: [{
            index: 0,
            id: "call_submit",
            function: { name: "submit_answer", arguments: "{\"answer\":\"complete draft" },
          }],
        } }],
        usage: { prompt_tokens: 20, completion_tokens: 8 },
      }]);
    });
  });

  const upstreamPort = await listen(upstream);
  await writeFile(apiKeyFile, "test-key\n", { mode: 0o600 });
  await writeFile(modelCatalogFile, JSON.stringify({ models: [{ slug: "Test-Model" }] }));
  await writeFile(registryFile, JSON.stringify({ providers: {}, models: {} }));

  const child = spawn(process.execPath, [proxyPath], {
    env: {
      ...process.env,
      RETRACE_PROXY_HOST: "127.0.0.1",
      RETRACE_PROXY_PORT: "0",
      RETRACE_READY_FILE: readyFile,
      RETRACE_UPSTREAM_BASE: `http://127.0.0.1:${upstreamPort}/v1`,
      RETRACE_API_KEY_FILE: apiKeyFile,
      RETRACE_MODEL_CATALOG_JSON: modelCatalogFile,
      RETRACE_REGISTRY_JSON: registryFile,
      RETRACE_AGENT_CHECK_FILE: path.join(tempDir, "agentcheck-disabled"),
      RETRACE_REQUIRE_CLIENT_TOOL: "1",
      RETRACE_CLIENT_TOOL_MAX_CONTINUES: "1",
      RETRACE_CLIENT_TOOL_ALLOWLIST: "submit_answer",
    },
    stdio: ["ignore", "ignore", "pipe"],
  });
  let stderr = "";
  child.stderr.on("data", (chunk) => { stderr += chunk; });

  try {
    const proxyPort = await waitForFile(readyFile, child);
    const response = await fetch(`http://127.0.0.1:${proxyPort}/v1/responses`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        model: "Test-Model",
        input: [{ type: "message", role: "user", content: "finish the job" }],
        tools: [
          {
            type: "function",
            name: "submit_answer",
            description: "Submit the final answer",
            parameters: { type: "object", properties: { answer: { type: "string" } } },
          },
          {
            type: "function",
            name: "unrelated_tool",
            description: "Must not be available",
            parameters: { type: "object", properties: {} },
          },
        ],
      }),
    });
    assert.equal(response.status, 200, stderr);
    const events = (await response.text())
      .split("\n")
      .filter((line) => line.startsWith("data: ") && line !== "data: [DONE]")
      .map((line) => JSON.parse(line.slice(6)));

    assert.equal(upstreamBodies.length, 2, stderr);
    for (const body of upstreamBodies) {
      assert.deepEqual(body.tools.map((tool) => tool.function.name), ["submit_answer"]);
    }
    assert(upstreamBodies[1].messages.some(
      (message) => message.role === "assistant" && message.content === "complete draft",
    ));
    assert(!events.some((event) => event.type === "response.output_text.delta"));
    assert(!events.some((event) => JSON.stringify(event).includes("unrelated_tool")));
    const completedArgs = events.find(
      (event) => event.type === "response.function_call_arguments.done",
    )?.arguments;
    assert.equal(completedArgs, "{\"answer\":\"complete draft\"}");
  } finally {
    child.kill("SIGTERM");
    await new Promise((resolve) => child.once("exit", resolve));
    await close(upstream);
    await rm(tempDir, { recursive: true, force: true });
  }
});
