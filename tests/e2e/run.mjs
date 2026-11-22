import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const REPO_ROOT = path.resolve(__dirname, '../..');

function resolveFluxwrightBin() {
  if (process.env.FLUXWRIGHT_BIN) {
    return process.env.FLUXWRIGHT_BIN;
  }
  const ext = process.platform === 'win32' ? '.exe' : '';
  const candidates = [
    path.join(REPO_ROOT, 'target', 'debug', `fluxwright${ext}`),
    path.join(REPO_ROOT, 'target', 'release', `fluxwright${ext}`),
  ];
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  throw new Error(
    `Could not find compiled fluxwright binary (checked ${candidates.join(', ')}; set FLUXWRIGHT_BIN or run cargo build -p fluxwright-cli)`,
  );
}

function resolveChromePath() {
  const fromEnv = process.env.FLUXWRIGHT_CHROMIUM || process.env.CHROME_PATH;
  if (fromEnv && fs.existsSync(fromEnv)) {
    return fromEnv;
  }
  const candidates =
    process.platform === 'win32'
      ? [
          'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
          'C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe',
        ]
      : process.platform === 'darwin'
        ? ['/Applications/Google Chrome.app/Contents/MacOS/Google Chrome']
        : ['/usr/bin/google-chrome', '/usr/bin/chromium', '/usr/bin/chromium-browser'];
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

function chromeLaunchOptions(chromePath) {
  const noSandbox =
    process.env.FLUXWRIGHT_NO_SANDBOX &&
    process.env.FLUXWRIGHT_NO_SANDBOX !== '0' &&
    process.env.FLUXWRIGHT_NO_SANDBOX.toLowerCase() !== 'false';
  const args = noSandbox ? ['--no-sandbox', '--disable-setuid-sandbox'] : [];
  return {
    ...(chromePath ? { executablePath: chromePath } : {}),
    headless: true,
    args,
  };
}

function getFreePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const addr = srv.address();
      srv.close((err) => {
        if (err) reject(err);
        else resolve(addr.port);
      });
    });
  });
}

function startFixtureServer() {
  const html = `<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>E2E Fixture</title></head>
<body>
  <main>
    <h1>Fluxwright E2E Fixture</h1>
    <p id="status">Ready</p>
  </main>
</body>
</html>`;

  const server = http.createServer((req, res) => {
    const url = new URL(req.url || '/', 'http://127.0.0.1');
    if (url.pathname === '/' || url.pathname === '/index.html') {
      res.writeHead(200, {
        'Content-Type': 'text/html; charset=utf-8',
        'Content-Length': Buffer.byteLength(html),
      });
      res.end(html);
      return;
    }
    res.writeHead(404, { 'Content-Type': 'text/plain' });
    res.end('not found');
  });

  return new Promise((resolve, reject) => {
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      resolve({
        url: `http://127.0.0.1:${port}`,
        close: () => new Promise((res) => server.close(() => res())),
      });
    });
  });
}

const MIME_TYPES = new Map([
  ['.html', 'text/html; charset=utf-8'],
  ['.js', 'application/javascript; charset=utf-8'],
  ['.mjs', 'application/javascript; charset=utf-8'],
  ['.css', 'text/css; charset=utf-8'],
  ['.json', 'application/json; charset=utf-8'],
  ['.svg', 'image/svg+xml'],
  ['.png', 'image/png'],
  ['.jpg', 'image/jpeg'],
  ['.jpeg', 'image/jpeg'],
  ['.ico', 'image/x-icon'],
  ['.woff2', 'font/woff2'],
  ['.woff', 'font/woff'],
  ['.txt', 'text/plain; charset=utf-8'],
]);

function startStaticServer(rootDir) {
  const resolvedRoot = path.resolve(rootDir);
  if (!fs.existsSync(path.join(resolvedRoot, 'index.html'))) {
    throw new Error(`Static site output not found at ${resolvedRoot} (run npm run build in www/)`);
  }

  const server = http.createServer((req, res) => {
    const url = new URL(req.url || '/', 'http://127.0.0.1');
    let relPath = decodeURIComponent(url.pathname);
    if (relPath === '/') relPath = '/index.html';

    const candidates = [
      path.resolve(resolvedRoot, '.' + relPath),
      path.resolve(resolvedRoot, '.' + relPath + '.html'),
      path.resolve(resolvedRoot, '.' + relPath, 'index.html'),
    ];

    let target = null;
    for (const candidate of candidates) {
      if (
        (candidate === resolvedRoot || candidate.startsWith(resolvedRoot + path.sep)) &&
        fs.existsSync(candidate) &&
        fs.statSync(candidate).isFile()
      ) {
        target = candidate;
        break;
      }
    }

    if (!target) {
      res.writeHead(404, { 'Content-Type': 'text/plain' });
      res.end('not found');
      return;
    }

    const ext = path.extname(target).toLowerCase();
    const contentType = MIME_TYPES.get(ext) || 'application/octet-stream';
    const data = fs.readFileSync(target);
    res.writeHead(200, {
      'Content-Type': contentType,
      'Content-Length': data.length,
    });
    res.end(data);
  });

  return new Promise((resolve, reject) => {
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      resolve({
        url: `http://127.0.0.1:${port}`,
        close: () => new Promise((res) => server.close(() => res())),
      });
    });
  });
}

async function waitForHealth(baseUrl, child, logs) {
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(
        `fluxwright serve exited prematurely with code ${child.exitCode}\n${logs.join('')}`,
      );
    }
    try {
      const res = await fetch(`${baseUrl}/healthz`);
      if (res.ok) return;
    } catch {
      // Server still binding
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`Timed out waiting for ${baseUrl}/healthz\n${logs.join('')}`);
}

async function stopChild(child) {
  if (!child || child.exitCode !== null) return;
  child.kill('SIGTERM');
  const exited = await Promise.race([
    new Promise((resolve) => child.once('exit', () => resolve(true))),
    new Promise((resolve) => setTimeout(() => resolve(false), 5000)),
  ]);
  if (!exited && child.exitCode === null) {
    child.kill('SIGKILL');
    await new Promise((resolve) => child.once('exit', () => resolve()));
  }
}

async function runCdpGatewayE2E(baseUrl, port, token, fixtureUrl) {
  console.log('[e2e] Running CDP Gateway E2E...');
  const unauthRes = await fetch(`http://127.0.0.1:${port}/json/version`);
  assert.equal(unauthRes.status, 401, '/json/version without auth must return 401');

  const verRes = await fetch(`http://127.0.0.1:${port}/json/version`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  assert.equal(verRes.status, 200, '/json/version with Bearer token must succeed');
  const verJson = await verRes.json();
  assert.equal(verJson.Browser, 'Fluxwright/Chrome');
  assert.equal(verJson.webSocketDebuggerUrl, `ws://127.0.0.1:${port}/cdp`);

  const verQueryRes = await fetch(`http://127.0.0.1:${port}/json/version?token=${token}`);
  assert.equal(verQueryRes.status, 200, '/json/version with ?token= must succeed');
  const verQueryJson = await verQueryRes.json();
  assert.equal(verQueryJson.webSocketDebuggerUrl, `ws://127.0.0.1:${port}/cdp`);

  const cdpUrl = `ws://127.0.0.1:${port}/cdp?token=${token}`;
  const clientA = await chromium.connectOverCDP(cdpUrl);
  const clientB = await chromium.connectOverCDP(cdpUrl);

  try {
    const ctxA = clientA.contexts()[0] ?? (await clientA.newContext());
    const ctxB = clientB.contexts()[0] ?? (await clientB.newContext());
    const pageA = ctxA.pages()[0] ?? (await ctxA.newPage());
    const pageB = ctxB.pages()[0] ?? (await ctxB.newPage());

    await Promise.all([pageA.goto(`${fixtureUrl}/`), pageB.goto(`${fixtureUrl}/`)]);

    await pageA.evaluate(() => {
      document.cookie = 'tenant=alpha; Path=/';
      localStorage.setItem('tenant_key', 'alpha-secret');
    });
    await pageB.evaluate(() => {
      document.cookie = 'tenant=beta; Path=/';
      localStorage.setItem('tenant_key', 'beta-secret');
    });

    const stateA = await pageA.evaluate(() => ({
      cookie: document.cookie,
      ls: localStorage.getItem('tenant_key'),
    }));
    const stateB = await pageB.evaluate(() => ({
      cookie: document.cookie,
      ls: localStorage.getItem('tenant_key'),
    }));

    assert.match(stateA.cookie, /tenant=alpha/);
    assert.doesNotMatch(stateA.cookie, /tenant=beta/);
    assert.equal(stateA.ls, 'alpha-secret');

    assert.match(stateB.cookie, /tenant=beta/);
    assert.doesNotMatch(stateB.cookie, /tenant=alpha/);
    assert.equal(stateB.ls, 'beta-secret');

    const cookiesA = await ctxA.cookies();
    const cookiesB = await ctxB.cookies();
    assert.equal(cookiesA.some((c) => c.name === 'tenant' && c.value === 'alpha'), true);
    assert.equal(cookiesA.some((c) => c.value === 'beta'), false);
    assert.equal(cookiesB.some((c) => c.name === 'tenant' && c.value === 'beta'), true);
    assert.equal(cookiesB.some((c) => c.value === 'alpha'), false);
  } finally {
    await Promise.allSettled([clientA.close(), clientB.close()]);
  }

  // Ensure server stats reflect clean lease teardown
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    const statsRes = await fetch(`${baseUrl}/v1/stats`, {
      headers: { Authorization: `Bearer ${token}` },
    });
    assert.equal(statsRes.status, 200);
    const stats = await statsRes.json();
    if (stats.active_leases === 0) {
      console.log('[e2e] CDP Gateway E2E passed.');
      return;
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error('Timed out waiting for CDP gateway leases to release after close');
}

async function runDashboardHitlE2E(baseUrl, token, fixtureUrl, chromePath) {
  console.log('[e2e] Running Live /dashboard HITL E2E...');
  const submitRes = await fetch(`${baseUrl}/v1/jobs`, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${token}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({
      timeout_ms: 120_000,
      steps: [
        { action: 'goto', url: `${fixtureUrl}/` },
        { action: 'pause_for_human', message: 'E2E HITL Approval', timeout_ms: 90_000 },
        { action: 'title' },
      ],
    }),
  });
  assert.equal(submitRes.status, 202, 'POST /v1/jobs must return 202');
  const submitted = await submitRes.json();
  const jobId = submitted.id;
  assert.ok(jobId, 'submitted job must include id');

  const browser = await chromium.launch(chromeLaunchOptions(chromePath));
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    const errors = [];
    page.on('pageerror', (err) => errors.push(`pageerror: ${err.message}`));
    page.on('console', (msg) => {
      if (msg.type() === 'error') {
        errors.push(`console.error: ${msg.text()}`);
      }
    });

    await page.goto(`${baseUrl}/dashboard`, { waitUntil: 'domcontentloaded' });

    // Authenticate via the native modal dialog
    const authDialog = page.locator('#auth-dialog[open]');
    await authDialog.waitFor({ state: 'visible', timeout: 10_000 });
    await page.locator('#token-input').fill(token);
    await page.locator('#auth-form button[type="submit"]').click();

    await page.waitForFunction(
      () => document.getElementById('conn-label')?.textContent === 'Connected (live)',
      undefined,
      { timeout: 15_000 },
    );

    // Wait for the live paused job to appear in Live Browser Sessions and click Watch Live
    const watchBtn = page.locator(`#live-body button[data-live-id="${jobId}"]`);
    await watchBtn.waitFor({ state: 'visible', timeout: 20_000 });
    await page.locator('#live-body .badge-paused').first().waitFor({ state: 'visible', timeout: 20_000 });
    await watchBtn.click();

    // Verify screencast frames render in #live-frame
    const liveDialog = page.locator('#live-dialog[open]');
    await liveDialog.waitFor({ state: 'visible', timeout: 10_000 });
    const liveFrame = page.locator('#live-frame');
    await liveFrame.waitFor({ state: 'visible', timeout: 15_000 });
    await page.waitForFunction(
      () => {
        const img = document.getElementById('live-frame');
        return (
          img &&
          !img.hidden &&
          img.src.startsWith('data:image/jpeg;base64,') &&
          img.complete &&
          img.naturalWidth > 0
        );
      },
      undefined,
      { timeout: 15_000 },
    );

    // Verify pause banner is visible and click Resume
    const pauseBanner = page.locator('#live-pause-banner');
    await pauseBanner.waitFor({ state: 'visible', timeout: 10_000 });
    const resumeBtn = page.locator('#btn-live-banner-resume');
    await resumeBtn.waitFor({ state: 'visible', timeout: 10_000 });
    await resumeBtn.click();

    // Poll job completion and assert { resumed: true }
    const deadline = Date.now() + 30_000;
    let finalJob = null;
    while (Date.now() < deadline) {
      const pollRes = await fetch(`${baseUrl}/v1/jobs/${jobId}`, {
        headers: { Authorization: `Bearer ${token}` },
      });
      assert.equal(pollRes.status, 200);
      const job = await pollRes.json();
      if (job.status === 'succeeded' || job.status === 'failed' || job.status === 'cancelled') {
        finalJob = job;
        break;
      }
      await new Promise((r) => setTimeout(r, 100));
    }

    assert.ok(finalJob, 'HITL job did not reach terminal state');
    assert.equal(finalJob.status, 'succeeded', `HITL job failed: ${JSON.stringify(finalJob)}`);
    assert.deepEqual(finalJob.results[1], { resumed: true });
    assert.equal(finalJob.results[2], 'E2E Fixture');
    assert.deepEqual(errors, [], `Unexpected dashboard errors: ${errors.join('; ')}`);
    console.log('[e2e] Live /dashboard HITL E2E passed.');
  } finally {
    await browser.close();
  }
}

async function exerciseTabs(page, sectionSelector, expectedTabs) {
  const section = page.locator(sectionSelector);
  await section.scrollIntoViewIfNeeded();
  const seenTexts = new Set();

  for (const title of expectedTabs) {
    const tab = section.getByRole('tab', { name: title, exact: true });
    await tab.waitFor({ state: 'visible', timeout: 10_000 });
    await tab.click();
    await page.waitForFunction(
      ({ sel, name }) => {
        const root = document.querySelector(sel);
        if (!root) return false;
        const buttons = Array.from(root.querySelectorAll('[role="tab"]'));
        const target = buttons.find((b) => b.textContent?.trim() === name);
        return target?.getAttribute('aria-selected') === 'true';
      },
      { sel: sectionSelector, name: title },
      { timeout: 5_000 },
    );

    const panelId = await tab.getAttribute('aria-controls');
    assert.ok(panelId, `Tab "${title}" in ${sectionSelector} missing aria-controls`);

    const panel = section.locator(`[id="${panelId}"]`);
    await panel.waitFor({ state: 'visible', timeout: 5_000 });
    const text = (await panel.innerText()).trim();
    assert.ok(text.length > 0, `Panel for tab "${title}" in ${sectionSelector} is empty`);
    seenTexts.add(text);

    const hiddenCheck = await section.evaluate(
      (root, activeId) => {
        const panels = Array.from(root.querySelectorAll('[role="tabpanel"]'));
        return panels.every((p) => (p.id === activeId ? !p.hidden : p.hidden));
      },
      panelId,
    );
    assert.equal(
      hiddenCheck,
      true,
      `Expected only panel ${panelId} visible in ${sectionSelector} for tab "${title}"`,
    );
  }

  assert.equal(
    seenTexts.size,
    expectedTabs.length,
    `Expected distinct panel content for every tab in ${sectionSelector}`,
  );
}

async function runSiteE2E(chromePath) {
  console.log('[e2e] Running www/ Site E2E...');
  const siteOutDir = path.join(REPO_ROOT, 'www', 'out');
  const siteServer = await startStaticServer(siteOutDir);
  const browser = await chromium.launch(chromeLaunchOptions(chromePath));

  try {
    const viewports = [
      { name: 'desktop', width: 1280, height: 800 },
      { name: 'mobile', width: 375, height: 812 },
    ];
    const codeTabs = ['TypeScript', 'Python', 'Rust', 'Server / CDP', 'MCP'];
    const installTabs = ['npm', 'pip', 'Rust', 'MCP', 'CLI', 'Docker'];

    for (const vp of viewports) {
      const context = await browser.newContext({ viewport: { width: vp.width, height: vp.height } });
      const page = await context.newPage();
      const consoleErrors = [];
      const pageErrors = [];
      const badResponses = [];

      page.on('console', (msg) => {
        if (msg.type() === 'error') {
          consoleErrors.push(msg.text());
        }
      });
      page.on('pageerror', (err) => {
        pageErrors.push(err.message);
      });
      page.on('response', (res) => {
        if (res.url().startsWith(siteServer.url) && res.status() >= 400) {
          badResponses.push(`${res.status()} ${res.url()}`);
        }
      });

      await page.goto(siteServer.url, { waitUntil: 'networkidle' });
      const title = await page.title();
      assert.match(title, /Fluxwright/i);

      await exerciseTabs(page, '#code', codeTabs);
      await exerciseTabs(page, '#install', installTabs);

      assert.deepEqual(
        consoleErrors,
        [],
        `Console errors on www/ (${vp.name}): ${consoleErrors.join('; ')}`,
      );
      assert.deepEqual(
        pageErrors,
        [],
        `Page errors on www/ (${vp.name}): ${pageErrors.join('; ')}`,
      );
      assert.deepEqual(
        badResponses,
        [],
        `Failed local asset responses on www/ (${vp.name}): ${badResponses.join('; ')}`,
      );

      await context.close();
      console.log(`[e2e] www/ Site E2E passed (${vp.name} ${vp.width}x${vp.height}).`);
    }
  } finally {
    await browser.close();
    await siteServer.close();
  }
}

async function main() {
  const fluxwrightBin = resolveFluxwrightBin();
  const chromePath = resolveChromePath();
  const token = crypto.randomBytes(32).toString('hex');
  const profilesDir = fs.mkdtempSync(path.join(os.tmpdir(), 'fluxwright-e2e-profiles-'));
  const fixture = await startFixtureServer();
  const port = await getFreePort();
  const baseUrl = `http://127.0.0.1:${port}`;
  const serverLogs = [];

  const serverProc = spawn(
    fluxwrightBin,
    [
      'serve',
      '--listen',
      `127.0.0.1:${port}`,
      '--allow-query-token',
      '--profiles-dir',
      profilesDir,
    ],
    {
      stdio: ['ignore', 'pipe', 'pipe'],
      env: {
        ...process.env,
        FLUXWRIGHT_SERVER_TOKEN: token,
        FLUXWRIGHT_NO_SANDBOX: process.env.FLUXWRIGHT_NO_SANDBOX || '1',
      },
    },
  );

  const capture = (chunk) => {
    serverLogs.push(chunk.toString('utf8'));
    if (serverLogs.length > 200) serverLogs.shift();
  };
  serverProc.stdout.on('data', capture);
  serverProc.stderr.on('data', capture);

  try {
    await waitForHealth(baseUrl, serverProc, serverLogs);
    await runCdpGatewayE2E(baseUrl, port, token, fixture.url);
    await runDashboardHitlE2E(baseUrl, token, fixture.url, chromePath);
    await runSiteE2E(chromePath);
  } catch (err) {
    console.error('[e2e] Server logs:\n' + serverLogs.join(''));
    throw err;
  } finally {
    await stopChild(serverProc);
    await fixture.close();
    fs.rmSync(profilesDir, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
