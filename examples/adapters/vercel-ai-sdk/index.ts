import { createMCPClient } from "@ai-sdk/mcp";
import { generateText, isStepCount, tool } from "ai";
import { z } from "zod";

interface ScrapeResponse {
  url: string;
  title: string;
  duration_ms: number;
  markdown?: string;
  html?: string;
  text?: string;
  snapshot?: unknown;
}

interface MapResponse {
  url: string;
  count: number;
  links: string[];
  duration_ms: number;
}

function requireEnv(name: string): string {
  const value = process.env[name]?.trim();
  if (!value) {
    throw new Error(`${name} environment variable is required`);
  }
  return value;
}

async function fluxwrightPost<T>(
  serverUrl: string,
  token: string,
  path: string,
  payload: Record<string, unknown>,
): Promise<T> {
  const response = await fetch(`${serverUrl}${path}`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify(payload),
  });
  if (!response.ok) {
    throw new Error(
      `POST ${path} failed (${response.status}): ${await response.text()}`,
    );
  }
  return (await response.json()) as T;
}

export function createFluxwrightTools(serverUrl: string, token: string) {
  return {
    fluxwrightScrape: tool({
      description:
        "Fetch a web page via Fluxwright POST /v1/scrape and return clean markdown, text, HTML, or an ARIA @eN snapshot.",
      inputSchema: z.object({
        url: z.string().url(),
        formats: z
          .array(z.enum(["markdown", "html", "text", "snapshot"]))
          .min(1)
          .max(4)
          .optional(),
        waitUntil: z
          .enum(["load", "domcontentloaded", "networkidle", "commit"])
          .optional(),
        resourcePreset: z
          .enum(["scraping", "minimal", "no_media"])
          .optional(),
      }),
      execute: async ({ url, formats, waitUntil, resourcePreset }) =>
        fluxwrightPost<ScrapeResponse>(serverUrl, token, "/v1/scrape", {
          url,
          ...(formats !== undefined ? { formats } : {}),
          ...(waitUntil !== undefined ? { wait_until: waitUntil } : {}),
          ...(resourcePreset !== undefined
            ? { resource_preset: resourcePreset }
            : {}),
        }),
    }),

    fluxwrightMap: tool({
      description:
        "Discover HTTP/HTTPS links on a page via Fluxwright POST /v1/map.",
      inputSchema: z.object({
        url: z.string().url(),
        sameOrigin: z.boolean().optional(),
        includeSubdomains: z.boolean().optional(),
        search: z.string().optional(),
        limit: z.number().int().min(1).max(1000).optional(),
      }),
      execute: async ({
        url,
        sameOrigin,
        includeSubdomains,
        search,
        limit,
      }) =>
        fluxwrightPost<MapResponse>(serverUrl, token, "/v1/map", {
          url,
          ...(sameOrigin !== undefined ? { same_origin: sameOrigin } : {}),
          ...(includeSubdomains !== undefined
            ? { include_subdomains: includeSubdomains }
            : {}),
          ...(search !== undefined ? { search } : {}),
          ...(limit !== undefined ? { limit } : {}),
        }),
    }),
  };
}

export async function runWithHttpTools(prompt: string): Promise<string> {
  const serverUrl = (
    process.env.FLUXWRIGHT_SERVER_URL ?? "http://127.0.0.1:7331"
  ).replace(/\/+$/, "");
  const token = requireEnv("FLUXWRIGHT_SERVER_TOKEN");
  const model = process.env.AI_MODEL ?? "anthropic/claude-sonnet-4-5";

  const { text } = await generateText({
    model,
    tools: createFluxwrightTools(serverUrl, token),
    stopWhen: isStepCount(4),
    prompt,
  });
  return text;
}

export async function runWithMcpClient(prompt: string): Promise<string> {
  const mcpUrl = process.env.FLUXWRIGHT_MCP_URL ?? "http://127.0.0.1:8090/mcp";
  const mcpToken = process.env.FLUXWRIGHT_MCP_TOKEN?.trim();
  const model = process.env.AI_MODEL ?? "anthropic/claude-sonnet-4-5";

  // Connects to `fluxwright-mcp --http 127.0.0.1:8090` over Streamable HTTP.
  const mcpClient = await createMCPClient({
    transport: {
      type: "http",
      url: mcpUrl,
      ...(mcpToken ? { headers: { Authorization: `Bearer ${mcpToken}` } } : {}),
    },
  });

  try {
    const tools = await mcpClient.tools();
    const { text } = await generateText({
      model,
      tools,
      stopWhen: isStepCount(6),
      prompt,
    });
    return text;
  } finally {
    await mcpClient.close();
  }
}

async function main(): Promise<void> {
  const mode = process.env.FLUXWRIGHT_MODE ?? "http";
  const prompt =
    process.env.PROMPT ??
    "Map links on https://example.com and scrape its main content in markdown.";

  const output =
    mode === "mcp"
      ? await runWithMcpClient(prompt)
      : await runWithHttpTools(prompt);
  console.log(output);
}

main().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
