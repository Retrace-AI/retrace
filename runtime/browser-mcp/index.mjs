#!/usr/bin/env node
// Retrace Browser MCP
// -------------------
// A Playwright-backed browser MCP built for vision / grounding models. Unlike
// @playwright/mcp, it enforces a fixed NORMALIZED coordinate contract:
//
//   * browser_screenshot always returns a NORM x NORM image (default 1000x1000).
//   * The model reasons and emits click coordinates in that 0..NORM space.
//   * This server maps those normalized coordinates back to REAL page pixels
//     (per-axis), so device pixel ratio (retina 2x) and viewport size are all
//     folded into one place. The model never deals with real pixels.
//
// Env:
//   RETRACE_BROWSER_NORM        normalized square size (default 1000)
//   RETRACE_BROWSER_VIEWPORT    "WxH" real viewport (default 1280x800)
//   RETRACE_BROWSER_HEADLESS=1  run headless (default: headed real Chrome)
//   RETRACE_BROWSER_PROFILE     persistent profile dir

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { chromium } from "playwright";
import sharp from "sharp";
import fs from "node:fs";

const HOME = process.env.HOME || "";
const NORM = Math.max(200, Number(process.env.RETRACE_BROWSER_NORM) || 1000);
const [VW, VH] = (process.env.RETRACE_BROWSER_VIEWPORT || "1280x800")
  .split("x")
  .map((n) => Math.max(320, Number(n) || 0));
const HEADLESS = process.env.RETRACE_BROWSER_HEADLESS === "1";
const PROFILE =
  process.env.RETRACE_BROWSER_PROFILE || `${HOME}/.retrace/browser-profile`;

let ctx = null;
let page = null;
// Real viewport (CSS pixels) captured at the last screenshot; the coordinate
// contract is anchored to this so a click uses the scale of what the model saw.
let real = { w: VW, h: VH };

function launchCtx() {
  return chromium.launchPersistentContext(PROFILE, {
    channel: "chrome",
    headless: HEADLESS,
    viewport: { width: VW, height: VH },
    deviceScaleFactor: 1, // capture in CSS pixels; screenshot space == click space
  });
}

function clearProfileLocks() {
  for (const f of ["SingletonLock", "SingletonSocket", "SingletonCookie"]) {
    try {
      fs.rmSync(`${PROFILE}/${f}`, { force: true });
    } catch {}
  }
}

let launchingPromise = null;

async function ensurePage() {
  if (page && !page.isClosed()) return page;
  // Serialize launches so concurrent tool calls never race two Chrome instances
  // onto the same profile (which triggers the ProcessSingleton lock error).
  if (launchingPromise) {
    await launchingPromise;
    if (page && !page.isClosed()) return page;
  }
  launchingPromise = (async () => {
  // This MCP owns the profile exclusively; a leftover lock means a prior
  // instance crashed. Clear it up front rather than only on failure.
  clearProfileLocks();
  try {
    ctx = await launchCtx();
  } catch (e) {
    // A crashed/killed prior instance leaves a stale SingletonLock. Clear it and
    // retry once so the browser recovers on its own.
    if (/ProcessSingleton|SingletonLock|already in use/i.test(String(e?.message || e))) {
      clearProfileLocks();
      ctx = await launchCtx();
    } else {
      throw e;
    }
  }
  page = ctx.pages()[0] || (await ctx.newPage());
  page.on("close", () => {
    page = null;
  });
  return page;
  })();
  try {
    return await launchingPromise;
  } finally {
    launchingPromise = null;
  }
}

// Normalized (0..NORM) -> real CSS pixels, per axis (inverts the fill resize).
function toReal(x, y) {
  return {
    x: (Number(x) / NORM) * real.w,
    y: (Number(y) / NORM) * real.h,
  };
}

async function normalizedScreenshot() {
  const p = await ensurePage();
  const vp = p.viewportSize() || { width: VW, height: VH };
  real = { w: vp.width, h: vp.height };
  const raw = await p.screenshot({ type: "png", scale: "css" });
  // Resize to exactly NORM x NORM (fill). Per-axis inverse in toReal() recovers
  // the exact real point, so the coordinate round-trip is exact.
  const png = await sharp(raw)
    .resize(NORM, NORM, { fit: "fill" })
    .png({ compressionLevel: 9 })
    .toBuffer();
  return png.toString("base64");
}

const TOOLS = [
  {
    name: "browser_navigate",
    description: "Open a URL in the browser. Follow with browser_screenshot to see the page.",
    inputSchema: {
      type: "object",
      properties: { url: { type: "string", description: "Absolute URL to open." } },
      required: ["url"],
    },
  },
  {
    name: "browser_screenshot",
    description: `Capture the current page as a ${NORM}x${NORM} normalized image. Emit all click/move coordinates in this 0..${NORM} (x) by 0..${NORM} (y) space; the server maps them to the real page.`,
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "browser_click",
    description: `Click at (x,y) given in the ${NORM}x${NORM} normalized image space (top-left origin). The server converts to real page pixels.`,
    inputSchema: {
      type: "object",
      properties: {
        x: { type: "number", description: `0..${NORM}` },
        y: { type: "number", description: `0..${NORM}` },
        button: { type: "string", enum: ["left", "right", "middle"] },
        doubleClick: { type: "boolean" },
      },
      required: ["x", "y"],
    },
  },
  {
    name: "browser_move",
    description: `Move the mouse to (x,y) in the ${NORM}x${NORM} normalized space (for hover).`,
    inputSchema: {
      type: "object",
      properties: { x: { type: "number" }, y: { type: "number" } },
      required: ["x", "y"],
    },
  },
  {
    name: "browser_type",
    description: "Type text into the currently focused element (click a field first).",
    inputSchema: {
      type: "object",
      properties: { text: { type: "string" }, submit: { type: "boolean", description: "Press Enter after typing." } },
      required: ["text"],
    },
  },
  {
    name: "browser_key",
    description: "Press a key or chord (e.g. Enter, Escape, ArrowDown, Control+A).",
    inputSchema: {
      type: "object",
      properties: { key: { type: "string" } },
      required: ["key"],
    },
  },
  {
    name: "browser_scroll",
    description: "Scroll by dx,dy pixels (positive dy scrolls down).",
    inputSchema: {
      type: "object",
      properties: { dx: { type: "number" }, dy: { type: "number" } },
    },
  },
  {
    name: "browser_snapshot",
    description: "Return the page URL, title, and visible text (cheap; no image). Use to read content without a screenshot.",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "browser_back",
    description: "Navigate back in history.",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "browser_wait",
    description: "Wait for up to 30 seconds (e.g. for a page to settle).",
    inputSchema: {
      type: "object",
      properties: { seconds: { type: "number" } },
      required: ["seconds"],
    },
  },
];

const server = new Server(
  { name: "retrace-browser", version: "1.0.0" },
  { capabilities: { tools: {} } },
);

server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: TOOLS }));

const text = (t) => ({ content: [{ type: "text", text: t }] });

server.setRequestHandler(CallToolRequestSchema, async (req) => {
  const { name, arguments: args = {} } = req.params;
  try {
    if (name === "browser_navigate") {
      const p = await ensurePage();
      await p.goto(String(args.url), { waitUntil: "domcontentloaded", timeout: 30000 });
      return text(`Navigated to ${p.url()}. Call browser_screenshot to see the ${NORM}x${NORM} page.`);
    }
    if (name === "browser_screenshot") {
      const data = await normalizedScreenshot();
      return {
        content: [
          { type: "text", text: `${NORM}x${NORM} normalized screenshot of ${page.url()} — emit click coordinates in 0..${NORM} space.` },
          { type: "image", data, mimeType: "image/png" },
        ],
      };
    }
    if (name === "browser_click") {
      const p = await ensurePage();
      const { x, y } = toReal(args.x, args.y);
      await p.mouse.click(x, y, {
        button: args.button || "left",
        clickCount: args.doubleClick ? 2 : 1,
      });
      await p.waitForTimeout(300);
      return text(`Clicked normalized (${args.x},${args.y}) -> real (${Math.round(x)},${Math.round(y)}).`);
    }
    if (name === "browser_move") {
      const p = await ensurePage();
      const { x, y } = toReal(args.x, args.y);
      await p.mouse.move(x, y);
      return text(`Moved to normalized (${args.x},${args.y}).`);
    }
    if (name === "browser_type") {
      const p = await ensurePage();
      await p.keyboard.type(String(args.text));
      if (args.submit) await p.keyboard.press("Enter");
      return text(`Typed ${String(args.text).length} chars${args.submit ? " + Enter" : ""}.`);
    }
    if (name === "browser_key") {
      const p = await ensurePage();
      await p.keyboard.press(String(args.key));
      return text(`Pressed ${args.key}.`);
    }
    if (name === "browser_scroll") {
      const p = await ensurePage();
      await p.mouse.wheel(Number(args.dx) || 0, Number(args.dy) || 0);
      await p.waitForTimeout(200);
      return text(`Scrolled dx=${args.dx || 0} dy=${args.dy || 0}.`);
    }
    if (name === "browser_snapshot") {
      const p = await ensurePage();
      const body = await p.evaluate(() => document.body?.innerText?.slice(0, 8000) || "");
      return text(`URL: ${p.url()}\nTitle: ${await p.title()}\n\n${body}`);
    }
    if (name === "browser_back") {
      const p = await ensurePage();
      await p.goBack({ waitUntil: "domcontentloaded" }).catch(() => {});
      return text(`Back to ${p.url()}.`);
    }
    if (name === "browser_wait") {
      const p = await ensurePage();
      await p.waitForTimeout(Math.min(30, Number(args.seconds) || 1) * 1000);
      return text("Done waiting.");
    }
    return { content: [{ type: "text", text: `Unknown tool: ${name}` }], isError: true };
  } catch (e) {
    return { content: [{ type: "text", text: `Error in ${name}: ${e?.message || String(e)}` }], isError: true };
  }
});

const transport = new StdioServerTransport();
await server.connect(transport);
