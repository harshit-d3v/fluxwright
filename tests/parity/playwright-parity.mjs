#!/usr/bin/env node
// Runs the exact same Playwright scripts against both Playwright and Fluxwright, changing only
// which `chromium` object is passed in. Usage:
//   node tests/parity/playwright-parity.mjs [--engine=both|playwright|fluxwright]
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import http from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";

const argEngine = process.argv.find((a) => a.startsWith("--engine="))?.split("=")[1] || "both";
const chrome = process.env.FLUXWRIGHT_CHROMIUM || process.env.CHROME || undefined;
const noSandbox = process.env.FLUXWRIGHT_NO_SANDBOX === "1";

function startFixtureServer() {
  return new Promise((resolve) => {
    const server = http.createServer((req, res) => {
      if (req.url === "/") {
        res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
        res.end(`<!doctype html>
<html>
  <head><title>Parity Home</title></head>
  <body>
    <h1>Welcome Fleet</h1>
    <label for="email">Work Email</label>
    <input id="email" type="email" placeholder="you@company.com" />
    <label><input id="agree" type="checkbox" /> Accept Terms</label>
    <select id="plan" aria-label="Plan">
      <option value="free">Free</option>
      <option value="pro">Pro</option>
    </select>
    <button id="submit" data-testid="save-btn">Save</button>
    <div id="status">idle</div>
    <div id="shadow-host"></div>
    <script>
      document.getElementById("submit").addEventListener("click", () => {
        const email = document.getElementById("email").value;
        const ok = document.getElementById("agree").checked;
        const plan = document.getElementById("plan").value;
        document.getElementById("status").textContent = email + ":" + ok + ":" + plan;
      });
      const root = document.getElementById("shadow-host").attachShadow({ mode: "open" });
      root.innerHTML = '<button id="sbtn">Shadow Click</button><span id="sout">off</span>';
      root.getElementById("sbtn").addEventListener("click", () => {
        root.getElementById("sout").textContent = "shadow-on";
      });
    </script>
  </body>
</html>`);
      } else if (req.url === "/iframe-host") {
        res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
        res.end(`<!doctype html><title>Host</title><iframe id="f" src="/iframe-child"></iframe>`);
      } else if (req.url === "/iframe-child") {
        res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
        res.end(`<!doctype html><button id="ibtn">Inside Frame</button><span id="iout">init</span>
          <script>document.getElementById("ibtn").onclick = () => document.getElementById("iout").textContent = "framed";</script>`);
      } else if (req.url === "/route-page") {
        res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
        res.end(`<!doctype html><div id="api">loading</div>
          <script>
            fetch("/api/data").then(r => r.json()).then(d => {
              document.getElementById("api").textContent = d.msg;
            });
          </script>`);
      } else if (req.url === "/api/data") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ msg: "real-server" }));
      } else if (req.url === "/set-auth") {
        res.writeHead(200, {
          "content-type": "text/html; charset=utf-8",
          "set-cookie": "fw_session=abc123; Path=/",
        });
        res.end(`<script>localStorage.setItem("role", "admin");</script><p>signed-in</p>`);
      } else if (req.url === "/check-auth") {
        res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
        res.end(`<div id="who"></div><script>
          document.getElementById("who").textContent = document.cookie + "|" + (localStorage.getItem("role") || "");
        </script>`);
      } else if (req.url === "/download-page") {
        res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
        res.end(`<a id="dl" href="/file.txt">Download</a>`);
      } else if (req.url === "/file.txt") {
        res.writeHead(200, {
          "content-type": "text/plain",
          "content-disposition": 'attachment; filename="hello.txt"',
        });
        res.end("fluxwright-parity-payload\n");
      } else if (req.url === "/dialog-page") {
        res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
        res.end(`<button id="ask">Ask</button><span id="ans"></span>
          <script>document.getElementById("ask").onclick = () => {
            document.getElementById("ans").textContent = prompt("Name?", "guest") || "none";
          };</script>`);
      } else if (req.url === "/clock-page") {
        res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
        res.end(`<span id="t">waiting</span>
          <script>setTimeout(() => { document.getElementById("t").textContent = "ticked"; }, 5000);</script>`);
      } else {
        res.writeHead(404);
        res.end("not found");
      }
    });
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      resolve({ server, baseUrl: `http://127.0.0.1:${port}` });
    });
  });
}

async function runParitySuite(label, chromium, baseUrl) {
  const workDir = await mkdtemp(join(tmpdir(), `fw-parity-${label}-`));
  const launchOpts = { headless: true };
  if (chrome) launchOpts.executablePath = chrome;
  if (noSandbox && label === "playwright") launchOpts.args = ["--no-sandbox"];
  const browser = await chromium.launch(launchOpts);

  try {
    // 1. Locators, form controls, Shadow DOM piercing
    {
      const ctx = await browser.newContext();
      const page = await ctx.newPage();
      await page.goto(`${baseUrl}/`);
      assert.equal(await page.title(), "Parity Home");
      assert.equal(await page.getByRole("heading", { name: "Welcome Fleet" }).textContent(), "Welcome Fleet");
      await page.getByLabel("Work Email").fill("ops@example.com");
      await page.getByRole("checkbox", { name: "Accept Terms" }).check();
      await page.locator("#plan").selectOption("pro");
      await page.getByTestId("save-btn").click();
      assert.equal(await page.locator("#status").textContent(), "ops@example.com:true:pro");

      // Open Shadow DOM button & text readback
      await page.getByRole("button", { name: "Shadow Click" }).click();
      assert.equal(await page.locator("#sout").textContent(), "shadow-on");
      await ctx.close();
    }

    // 2. Iframe locator
    {
      const ctx = await browser.newContext();
      const page = await ctx.newPage();
      await page.goto(`${baseUrl}/iframe-host`);
      const frame = page.frameLocator("#f");
      await frame.getByRole("button", { name: "Inside Frame" }).click();
      assert.equal(await frame.locator("#iout").textContent(), "framed");
      await ctx.close();
    }

    // 3. Route interception + waitForResponse
    {
      const ctx = await browser.newContext();
      const page = await ctx.newPage();
      await page.route("**/api/data", (route) =>
        route.fulfill({ json: { msg: "mocked-route" } }),
      );
      const resPromise = page.waitForResponse("**/api/data");
      await page.goto(`${baseUrl}/route-page`);
      const res = await resPromise;
      assert.equal(res.status(), 200);
      assert.equal((await res.json()).msg, "mocked-route");
      for (let i = 0; i < 20; i++) {
        if ((await page.locator("#api").textContent()) === "mocked-route") break;
        await page.waitForTimeout(25);
      }
      assert.equal(await page.locator("#api").textContent(), "mocked-route");
      await ctx.close();
    }

    // 4. Context isolation + storageState roundtrip
    {
      const statePath = join(workDir, "state.json");
      const ctx1 = await browser.newContext();
      const p1 = await ctx1.newPage();
      await p1.goto(`${baseUrl}/set-auth`);
      await ctx1.storageState({ path: statePath });
      await ctx1.close();

      // Fresh context without state must not see cookies/localStorage
      const ctxIso = await browser.newContext();
      const pIso = await ctxIso.newPage();
      await pIso.goto(`${baseUrl}/check-auth`);
      assert.equal(await pIso.locator("#who").textContent(), "|");
      await ctxIso.close();

      // Restored context receives both cookie and localStorage
      const ctx2 = await browser.newContext({ storageState: statePath });
      const p2 = await ctx2.newPage();
      await p2.goto(`${baseUrl}/check-auth`);
      const restored = await p2.locator("#who").textContent();
      assert.ok(restored.includes("fw_session=abc123"), `missing cookie: ${restored}`);
      assert.ok(restored.endsWith("|admin"), `missing localStorage: ${restored}`);
      await ctx2.close();
    }

    // 5. Dialog handling + File download
    {
      const ctx = await browser.newContext();
      const page = await ctx.newPage();
      page.on("dialog", async (dialog) => {
        assert.equal(dialog.type(), "prompt");
        await dialog.accept("flux-agent");
      });
      await page.goto(`${baseUrl}/dialog-page`);
      await page.locator("#ask").click();
      assert.equal(await page.locator("#ans").textContent(), "flux-agent");

      await page.goto(`${baseUrl}/download-page`);
      const [download] = await Promise.all([
        page.waitForEvent("download"),
        page.locator("#dl").click(),
      ]);
      assert.equal(download.suggestedFilename(), "hello.txt");
      const outPath = join(workDir, "hello.txt");
      await download.saveAs(outPath);
      assert.equal(await readFile(outPath, "utf8"), "fluxwright-parity-payload\n");
      await ctx.close();
    }

    // 6. Clock mocking + evaluate + screenshot + PDF
    {
      const ctx = await browser.newContext();
      const page = await ctx.newPage();
      await page.clock.install();
      await page.goto(`${baseUrl}/clock-page`);
      assert.equal(await page.locator("#t").textContent(), "waiting");
      await page.clock.fastForward(5000);
      assert.equal(await page.locator("#t").textContent(), "ticked");

      const sum = await page.evaluate(([a, b]) => a + b, [19, 23]);
      assert.equal(sum, 42);

      const png = await page.screenshot();
      assert.equal(png.subarray(0, 4).toString("hex"), "89504e47");
      const pdf = await page.pdf({ format: "A4" });
      assert.equal(pdf.subarray(0, 5).toString("latin1"), "%PDF-");
      await ctx.close();
    }

    console.log(`PASS parity suite (${label}): 6/6 suites matched`);
  } finally {
    await browser.close();
    await rm(workDir, { recursive: true, force: true });
  }
}

async function resolvePlaywright() {
  for (const spec of ["playwright", "../../benchmarks/node_modules/playwright/index.mjs"]) {
    try {
      const m = await import(spec);
      if (m.chromium) return m.chromium;
    } catch {
      // try next path
    }
  }
  return null;
}

async function resolveFluxwright() {
  for (const spec of ["fluxwright", "../../bindings/node/addon.js"]) {
    try {
      const m = await import(spec);
      const fw = m.default || m;
      if (fw.chromium) return fw.chromium;
    } catch {
      // try next path
    }
  }
  return null;
}

const { server, baseUrl } = await startFixtureServer();
try {
  if (argEngine === "both" || argEngine === "playwright") {
    const pw = await resolvePlaywright();
    if (!pw) throw new Error("playwright package not found");
    await runParitySuite("playwright", pw, baseUrl);
  }
  if (argEngine === "both" || argEngine === "fluxwright") {
    const fw = await resolveFluxwright();
    if (!fw) throw new Error("fluxwright native binding not found");
    await runParitySuite("fluxwright", fw, baseUrl);
  }
} finally {
  server.close();
}
