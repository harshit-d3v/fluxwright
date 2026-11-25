import { Stagehand } from "@browserbasehq/stagehand";
import { z } from "zod";

interface ManagedSession {
  id: string;
  cdp_ws_url: string;
  live_view_url: string;
}

function requireEnv(name: string): string {
  const value = process.env[name]?.trim();
  if (!value) {
    throw new Error(`${name} environment variable is required`);
  }
  return value;
}

async function createSession(
  serverUrl: string,
  token: string,
): Promise<ManagedSession> {
  const response = await fetch(`${serverUrl}/v1/sessions`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      ttl_secs: 300,
      keep_alive: false,
      viewport: { width: 1280, height: 720 },
      live_view_mode: "read_only",
    }),
  });
  if (!response.ok) {
    throw new Error(
      `POST /v1/sessions failed (${response.status}): ${await response.text()}`,
    );
  }
  return (await response.json()) as ManagedSession;
}

async function closeSession(
  serverUrl: string,
  token: string,
  sessionId: string,
): Promise<void> {
  const response = await fetch(`${serverUrl}/v1/sessions/${sessionId}`, {
    method: "DELETE",
    headers: { Authorization: `Bearer ${token}` },
  });
  if (!response.ok && response.status !== 404) {
    throw new Error(
      `DELETE /v1/sessions/${sessionId} failed (${response.status}): ${await response.text()}`,
    );
  }
}

async function main(): Promise<void> {
  const serverUrl = (
    process.env.FLUXWRIGHT_SERVER_URL ?? "http://127.0.0.1:7331"
  ).replace(/\/+$/, "");
  const token = requireEnv("FLUXWRIGHT_SERVER_TOKEN");
  const targetUrl = process.env.TARGET_URL ?? "https://example.com";
  const model = process.env.STAGEHAND_MODEL ?? "openai/gpt-4o";

  const session = await createSession(serverUrl, token);
  const liveViewUrl = new URL(session.live_view_url, serverUrl).toString();
  console.log(`Session created: ${session.id}`);
  console.log(`Live View URL:   ${liveViewUrl}`);

  // Stagehand v3 attaches to an external CDP endpoint via `env: "LOCAL"` with
  // `localBrowserLaunchOptions.cdpUrl` and passes Bearer auth via `cdpHeaders`.
  const stagehand = new Stagehand({
    env: "LOCAL",
    localBrowserLaunchOptions: {
      cdpUrl: session.cdp_ws_url,
      cdpHeaders: { Authorization: `Bearer ${token}` },
    },
    model,
    verbose: 0,
  });

  try {
    await stagehand.init();
    const page =
      stagehand.context.activePage() ?? stagehand.context.pages()[0];
    if (!page) {
      throw new Error("No initial page available in Stagehand context");
    }

    await page.goto(targetUrl);
    const action = await stagehand.act(
      "Click the main informational link on the page",
    );
    const extracted = await stagehand.extract(
      "Extract the page heading and a short one-sentence summary",
      z.object({
        heading: z.string(),
        summary: z.string(),
      }),
    );

    console.log("Action result:", action);
    console.log("Extracted data:", extracted);
  } finally {
    await stagehand.close().catch(() => undefined);
    await closeSession(serverUrl, token, session.id);
    console.log(`Session closed:  ${session.id}`);
  }
}

main().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
