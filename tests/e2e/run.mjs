import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
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

function resolveFluxwrightMcpBin() {
  if (process.env.FLUXWRIGHT_MCP_BIN) {
    return process.env.FLUXWRIGHT_MCP_BIN;
  }
  const ext = process.platform === 'win32' ? '.exe' : '';
  const candidates = [
    path.join(REPO_ROOT, 'target', 'debug', `fluxwright-mcp${ext}`),
    path.join(REPO_ROOT, 'target', 'release', `fluxwright-mcp${ext}`),
  ];
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  console.log('[e2e] Building fluxwright-mcp binary via cargo build -p fluxwright-mcp...');
  const built = spawnSync('cargo', ['build', '-p', 'fluxwright-mcp'], {
    cwd: REPO_ROOT,
    stdio: 'inherit',
  });
  if (built.status !== 0) {
    throw new Error(`cargo build -p fluxwright-mcp failed with status ${built.status}`);
  }
  if (fs.existsSync(candidates[0])) {
    return candidates[0];
  }
  throw new Error(
    `Could not find compiled fluxwright-mcp binary after build (checked ${candidates.join(', ')})`,
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

const ONE_BY_ONE_PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=',
  'base64',
);

function startFixtureServer() {
  const html = `<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>E2E Fixture</title></head>
<body>
  <main>
    <h1>Fluxwright E2E Fixture</h1>
    <p id="status">Ready</p>
    <a id="dl-link" href="/download-test" download="escape.txt">Download</a>
  </main>
</body>
</html>`;

  const hitlHtml = `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>HITL Fixture</title>
  <style>
    * { box-sizing: border-box; }
    html, body { margin: 0; padding: 0; width: 100%; height: 100%; background: #ffffff; color: #111; font-family: sans-serif; }
    #human-input {
      position: fixed; left: 40px; top: 40px; width: 400px; height: 60px;
      font-size: 22px; padding: 8px 12px; border: 2px solid #222;
    }
    #human-submit {
      position: fixed; left: 40px; top: 130px; width: 220px; height: 60px;
      font-size: 20px; background: #1d4ed8; color: #fff; border: none; cursor: pointer;
    }
    #human-result {
      position: fixed; left: 40px; top: 220px; font-size: 20px;
    }
  </style>
</head>
<body>
  <input id="human-input" type="text" autocomplete="off" />
  <button id="human-submit" type="button">Submit</button>
  <div id="human-result">PENDING</div>
  <script>
    document.getElementById('human-submit').addEventListener('click', function () {
      var val = document.getElementById('human-input').value;
      document.getElementById('human-result').textContent = 'SUBMITTED:' + val;
    });
  </script>
</body>
</html>`;

  const scrapingHtml = `<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Scraping Preset Fixture</title></head>
<body>
  <h1>Scraping Preset</h1>
  <img id="pic" src="/asset.png" alt="blocked asset" />
</body>
</html>`;

  let assetHits = 0;
  let downloadHits = 0;

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
    if (url.pathname === '/hitl') {
      res.writeHead(200, {
        'Content-Type': 'text/html; charset=utf-8',
        'Content-Length': Buffer.byteLength(hitlHtml),
      });
      res.end(hitlHtml);
      return;
    }
    if (url.pathname === '/scraping-test') {
      res.writeHead(200, {
        'Content-Type': 'text/html; charset=utf-8',
        'Content-Length': Buffer.byteLength(scrapingHtml),
      });
      res.end(scrapingHtml);
      return;
    }
    if (url.pathname === '/asset.png') {
      assetHits += 1;
      res.writeHead(200, {
        'Content-Type': 'image/png',
        'Content-Length': ONE_BY_ONE_PNG.length,
        'Cache-Control': 'no-store',
      });
      res.end(ONE_BY_ONE_PNG);
      return;
    }
    if (url.pathname === '/download-test') {
      downloadHits += 1;
      const body = 'blocked-host-write';
      res.writeHead(200, {
        'Content-Type': 'application/octet-stream',
        'Content-Disposition': 'attachment; filename="escape.txt"',
        'Content-Length': Buffer.byteLength(body),
        'Cache-Control': 'no-store',
      });
      res.end(body);
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
        getAssetHits: () => assetHits,
        resetAssetHits: () => {
          assetHits = 0;
        },
        getDownloadHits: () => downloadHits,
        resetDownloadHits: () => {
          downloadHits = 0;
        },
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

function rawWsUpgradeStatus(port, reqPath) {
  return new Promise((resolve, reject) => {
    const req = http.request({
      host: '127.0.0.1',
      port,
      path: reqPath,
      method: 'GET',
      headers: {
        Connection: 'Upgrade',
        Upgrade: 'websocket',
        'Sec-WebSocket-Version': '13',
        'Sec-WebSocket-Key': crypto.randomBytes(16).toString('base64'),
      },
    });
    req.on('response', (res) => {
      res.resume();
      resolve(res.statusCode);
    });
    req.on('upgrade', (_res, socket) => {
      socket.destroy();
      reject(new Error(`Unexpected WebSocket upgrade succeeded for ${reqPath}`));
    });
    req.on('error', reject);
    req.end();
  });
}

async function openRawCdp(wsUrl) {
  const ws = new WebSocket(wsUrl);
  let nextId = 1;
  const pending = new Map();
  ws.addEventListener('message', (ev) => {
    const raw = typeof ev.data === 'string' ? ev.data : String(ev.data);
    const msg = JSON.parse(raw);
    if (msg.id && pending.has(msg.id)) {
      const { resolve } = pending.get(msg.id);
      pending.delete(msg.id);
      resolve(msg);
    }
  });
  await new Promise((resolve, reject) => {
    const onOpen = () => {
      ws.removeEventListener('error', onErr);
      resolve();
    };
    const onErr = (err) => {
      ws.removeEventListener('open', onOpen);
      reject(new Error(`Raw CDP WebSocket open failed: ${err?.message || 'error'}`));
    };
    ws.addEventListener('open', onOpen, { once: true });
    ws.addEventListener('error', onErr, { once: true });
  });
  return {
    send(method, params = {}, sessionId = undefined) {
      const id = nextId++;
      const payload = { id, method, params };
      if (sessionId) payload.sessionId = sessionId;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(id);
          reject(new Error(`Raw CDP call timed out: ${method}`));
        }, 10_000);
        pending.set(id, {
          resolve: (msg) => {
            clearTimeout(timer);
            resolve(msg);
          },
        });
        ws.send(JSON.stringify(payload));
      });
    },
    close() {
      return new Promise((resolve) => {
        if (ws.readyState === WebSocket.CLOSED) return resolve();
        ws.addEventListener('close', () => resolve(), { once: true });
        ws.close();
      });
    },
  };
}

async function runCdpGatewayE2E(baseUrl, port, token, fixture) {
  console.log('[e2e] Running CDP Gateway E2E...');
  const fixtureUrl = fixture.url;
  const unauthRes = await fetch(`http://127.0.0.1:${port}/json/version`);
  assert.equal(unauthRes.status, 401, '/json/version without auth must return 401');

  const unauthWsStatus = await rawWsUpgradeStatus(port, '/cdp');
  assert.equal(unauthWsStatus, 401, '/cdp WebSocket upgrade without auth must return 401');

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

  // Test resource_preset=scraping on both primary context and child newContext(),
  // verifying images are blocked while JS confirm() dialogs are NOT auto-dismissed.
  fixture.resetAssetHits();
  const presetClient = await chromium.connectOverCDP(
    `ws://127.0.0.1:${port}/cdp?resource_preset=scraping&token=${token}`,
  );
  try {
    const primaryCtx = presetClient.contexts()[0] ?? (await presetClient.newContext());
    const primaryPage = primaryCtx.pages()[0] ?? (await primaryCtx.newPage());
    await primaryPage.goto(`${fixtureUrl}/scraping-test`);
    assert.equal(
      fixture.getAssetHits(),
      0,
      'resource_preset=scraping must block image requests in primary context',
    );

    let primaryDialogMsg = null;
    primaryPage.once('dialog', async (d) => {
      primaryDialogMsg = d.message();
      await d.accept();
    });
    const primaryConfirmed = await primaryPage.evaluate(() => window.confirm('primary-dialog-ok'));
    assert.equal(primaryDialogMsg, 'primary-dialog-ok');
    assert.equal(
      primaryConfirmed,
      true,
      'confirm() dialog must be accepted by Playwright, not auto-dismissed by engine',
    );

    const childCtx = await presetClient.newContext();
    const childPage = await childCtx.newPage();
    await childPage.goto(`${fixtureUrl}/scraping-test`);
    assert.equal(
      fixture.getAssetHits(),
      0,
      'resource_preset=scraping must also block image requests in child newContext()',
    );

    let childDialogMsg = null;
    childPage.once('dialog', async (d) => {
      childDialogMsg = d.message();
      await d.accept();
    });
    const childConfirmed = await childPage.evaluate(() => window.confirm('child-dialog-ok'));
    assert.equal(childDialogMsg, 'child-dialog-ok');
    assert.equal(
      childConfirmed,
      true,
      'confirm() dialog in child context must be accepted by Playwright, not auto-dismissed',
    );
  } finally {
    await presetClient.close();
  }

  // Test raw CDP security boundaries and cross-tenant isolation.
  const rawA = await openRawCdp(`ws://127.0.0.1:${port}/cdp?initial_page=true&token=${token}`);
  const rawB = await openRawCdp(`ws://127.0.0.1:${port}/cdp?token=${token}`);
  const forbiddenDlDir = fs.mkdtempSync(path.join(os.tmpdir(), 'fluxwright-e2e-forbidden-dl-'));
  try {
    const targetsResp = await rawA.send('Target.getTargets');
    const targetId = targetsResp.result?.targetInfos?.[0]?.targetId;
    assert.ok(targetId, `Expected initial target in rawA: ${JSON.stringify(targetsResp)}`);

    const attachResp = await rawA.send('Target.attachToTarget', { targetId, flatten: true });
    const sessionId = attachResp.result?.sessionId;
    assert.ok(sessionId, `Expected sessionId from attachToTarget: ${JSON.stringify(attachResp)}`);

    const dlBadType = await rawA.send('Browser.setDownloadBehavior', {
      behavior: 'allow',
      downloadPath: 42,
    });
    assert.equal(
      dlBadType.error?.code,
      -32602,
      `Browser.setDownloadBehavior with non-string downloadPath must return -32602: ${JSON.stringify(dlBadType)}`,
    );

    // Caller-supplied downloadPath is clamped to the lease's ephemeral dir (never written to host path).
    const dlBrowser = await rawA.send('Browser.setDownloadBehavior', {
      behavior: 'allow',
      downloadPath: forbiddenDlDir,
    });
    assert.equal(dlBrowser.error, undefined, `Browser.setDownloadBehavior clamped call failed: ${JSON.stringify(dlBrowser)}`);

    const dlPage = await rawA.send(
      'Page.setDownloadBehavior',
      { behavior: 'allow', downloadPath: forbiddenDlDir },
      sessionId,
    );
    assert.equal(dlPage.error, undefined, `Page.setDownloadBehavior clamped call failed: ${JSON.stringify(dlPage)}`);

    fixture.resetDownloadHits();
    await rawA.send('Page.navigate', { url: `${fixtureUrl}/` }, sessionId);
    const navDeadline = Date.now() + 10_000;
    while (Date.now() < navDeadline) {
      const ready = await rawA.send(
        'Runtime.evaluate',
        { expression: "document.readyState === 'complete' && !!document.getElementById('dl-link')", returnByValue: true },
        sessionId,
      );
      if (ready.result?.result?.value === true) break;
      await new Promise((r) => setTimeout(r, 30));
    }
    await rawA.send(
      'Runtime.evaluate',
      { expression: "document.getElementById('dl-link').click()", userGesture: true },
      sessionId,
    );
    const dlDeadline = Date.now() + 10_000;
    while (Date.now() < dlDeadline && fixture.getDownloadHits() === 0) {
      await new Promise((r) => setTimeout(r, 30));
    }
    assert.ok(fixture.getDownloadHits() >= 1, 'Expected download fixture endpoint to be hit');
    await new Promise((r) => setTimeout(r, 250));
    assert.deepEqual(
      fs.readdirSync(forbiddenDlDir),
      [],
      'Caller-supplied downloadPath must never receive downloaded files',
    );

    const fileInput = await rawA.send(
      'DOM.setFileInputFiles',
      { files: ['/etc/passwd'], nodeId: 1 },
      sessionId,
    );
    assert.equal(
      fileInput.error?.code,
      -32000,
      `DOM.setFileInputFiles over /cdp must be rejected: ${JSON.stringify(fileInput)}`,
    );

    const fileNav = await rawA.send('Page.navigate', { url: 'file:///etc/hosts' }, sessionId);
    assert.equal(
      fileNav.error?.code,
      -32000,
      `Page.navigate to file:/// must be rejected: ${JSON.stringify(fileNav)}`,
    );

    const ssrfNav = await rawA.send(
      'Page.navigate',
      { url: 'http://169.254.169.254/latest/meta-data/' },
      sessionId,
    );
    assert.equal(
      ssrfNav.error?.code,
      -32000,
      `Page.navigate to link-local metadata IP must be rejected: ${JSON.stringify(ssrfNav)}`,
    );

    const crossAttach = await rawB.send('Target.attachToTarget', { targetId, flatten: true });
    assert.equal(
      crossAttach.error?.code,
      -32000,
      `Cross-tenant Target.attachToTarget must be rejected: ${JSON.stringify(crossAttach)}`,
    );
  } finally {
    await Promise.allSettled([rawA.close(), rawB.close()]);
    fs.rmSync(forbiddenDlDir, { recursive: true, force: true });
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

async function runDashboardHitlE2E(baseUrl, port, token, fixtureUrl, chromePath) {
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
        { action: 'goto', url: `${fixtureUrl}/hitl` },
        { action: 'pause_for_human', message: 'E2E HITL Form Input', timeout_ms: 90_000 },
        { action: 'text_content', selector: '#human-result' },
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
        const locUrl = msg.location()?.url || '';
        if (locUrl.endsWith('/favicon.ico')) return;
        errors.push(`console.error (${locUrl}): ${msg.text()}`);
      }
    });
    page.on('response', (res) => {
      if (res.status() >= 400 && !res.url().endsWith('/favicon.ico')) {
        errors.push(`http ${res.status()}: ${res.url()}`);
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

    // Verify #meter-mem is wired to memory_ceiling_bytes / memory_ceiling_mb
    await page.waitForFunction(
      () => {
        const el = document.getElementById('meter-mem');
        return el && /^\d+(?:\.\d+)?%$/.test(el.style.width || '');
      },
      undefined,
      { timeout: 10_000 },
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
        const meta = document.getElementById('live-meta')?.textContent?.trim() || '';
        return (
          img &&
          !img.hidden &&
          img.src.startsWith('data:image/jpeg;base64,') &&
          img.complete &&
          img.naturalWidth > 0 &&
          /^\d+(?:\.\d+)?x\d+(?:\.\d+)?$/.test(meta)
        );
      },
      undefined,
      { timeout: 15_000 },
    );

    // Verify pause banner is visible
    const pauseBanner = page.locator('#live-pause-banner');
    await pauseBanner.waitFor({ state: 'visible', timeout: 10_000 });

    // Interact with the remote page through #live-frame mouse clicks and #live-stage keyboard input:
    // #human-input is fixed at (40, 40, 400x60) -> center (240, 70)
    // #human-submit is fixed at (40, 130, 220x60) -> center (150, 160)
    const metaText = (await page.locator('#live-meta').innerText()).trim();
    const metaMatch = /^(\d+(?:\.\d+)?)x(\d+(?:\.\d+)?)$/.exec(metaText);
    assert.ok(metaMatch, `Expected #live-meta WxH dimensions, got "${metaText}"`);
    const frameWidth = Number(metaMatch[1]);
    const frameHeight = Number(metaMatch[2]);

    const clickRemote = async (rx, ry) => {
      const box = await liveFrame.boundingBox();
      assert.ok(box && box.width > 0 && box.height > 0, 'Expected visible #live-frame bounding box');
      const cx = box.x + (rx / frameWidth) * box.width;
      const cy = box.y + (ry / frameHeight) * box.height;
      await page.mouse.click(cx, cy);
    };

    await clickRemote(240, 70);
    const activeId = await page.evaluate(() => document.activeElement?.id || '');
    assert.equal(activeId, 'live-stage', 'Clicking #live-frame must focus #live-stage for keyboard forwarding');
    await new Promise((r) => setTimeout(r, 200));

    await page.keyboard.type('hitl-ok-99', { delay: 25 });
    await new Promise((r) => setTimeout(r, 200));

    await clickRemote(150, 160);
    await new Promise((r) => setTimeout(r, 300));

    // Resume via the HITL banner button
    const resumeBtn = page.locator('#btn-live-banner-resume');
    await resumeBtn.waitFor({ state: 'visible', timeout: 10_000 });
    await resumeBtn.click();

    // Poll job completion and assert human-entered DOM value was read by the next step
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
    assert.equal(
      finalJob.results[2],
      'SUBMITTED:hitl-ok-99',
      `Expected job next step to observe HITL-typed form value, got: ${JSON.stringify(finalJob.results)}`,
    );
    assert.equal(finalJob.results[3], 'HITL Fixture');

    await page.locator('#btn-live-close').click();
    await page.waitForFunction(
      () => !document.getElementById('live-dialog')?.open,
      undefined,
      { timeout: 5_000 },
    );

    // Verify a /cdp live session appears in the dashboard Live Browser Sessions table and closes cleanly.
    const cdpLiveClient = await chromium.connectOverCDP(
      `ws://127.0.0.1:${port}/cdp?initial_page=true&token=${token}`,
    );
    try {
      const ctx = cdpLiveClient.contexts()[0] ?? (await cdpLiveClient.newContext());
      const cdpPage = ctx.pages()[0] ?? (await ctx.newPage());
      await cdpPage.goto(`${fixtureUrl}/`);

      await page.waitForFunction(
        () => {
          const section = document.getElementById('live-section');
          const rows = Array.from(document.querySelectorAll('#live-body tr'));
          return (
            section &&
            !section.hidden &&
            rows.some((r) => r.children[1]?.textContent?.trim() === 'CDP')
          );
        },
        undefined,
        { timeout: 15_000 },
      );
    } finally {
      await cdpLiveClient.close();
    }

    await page.waitForFunction(
      () => {
        const section = document.getElementById('live-section');
        if (!section) return false;
        if (section.hidden) return true;
        const rows = Array.from(document.querySelectorAll('#live-body tr'));
        return !rows.some((r) => r.children[1]?.textContent?.trim() === 'CDP');
      },
      undefined,
      { timeout: 15_000 },
    );

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

      // Assert zero horizontal overflow at this viewport
      const overflow = await page.evaluate(() => ({
        scrollWidth: document.documentElement.scrollWidth,
        innerWidth: window.innerWidth,
      }));
      assert.ok(
        overflow.scrollWidth <= overflow.innerWidth,
        `Horizontal overflow on www/ (${vp.name}): scrollWidth=${overflow.scrollWidth} > innerWidth=${overflow.innerWidth}`,
      );

      // Assert all in-page # anchor links point to existing element IDs
      const hashCheck = await page.evaluate(() => {
        const anchors = Array.from(document.querySelectorAll('a[href^="#"]'));
        const missing = [];
        for (const a of anchors) {
          const href = a.getAttribute('href') || '';
          if (href === '#' || href.length <= 1) continue;
          const targetId = decodeURIComponent(href.slice(1));
          if (!document.getElementById(targetId)) {
            missing.push(href);
          }
        }
        return { total: anchors.length, missing };
      });
      assert.ok(hashCheck.total > 0, `Expected at least one # anchor link on www/ (${vp.name})`);
      assert.deepEqual(
        hashCheck.missing,
        [],
        `Broken # anchor links on www/ (${vp.name}): ${hashCheck.missing.join(', ')}`,
      );

      await exerciseTabs(page, '#code', codeTabs);
      await exerciseTabs(page, '#install', installTabs);

      // Re-verify zero horizontal overflow after exercising tabs
      const overflowAfterTabs = await page.evaluate(() => ({
        scrollWidth: document.documentElement.scrollWidth,
        innerWidth: window.innerWidth,
      }));
      assert.ok(
        overflowAfterTabs.scrollWidth <= overflowAfterTabs.innerWidth,
        `Horizontal overflow after tabs on www/ (${vp.name}): scrollWidth=${overflowAfterTabs.scrollWidth} > innerWidth=${overflowAfterTabs.innerWidth}`,
      );

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

async function parseMcpHttpResponse(res, expectedId) {
  const contentType = res.headers.get('content-type') || '';
  const body = await res.text();
  if (contentType.includes('application/json')) {
    return JSON.parse(body);
  }
  const messages = [];
  let dataLines = [];
  for (const line of body.split(/\r?\n/)) {
    if (line.startsWith('data:')) {
      dataLines.push(line.slice(5).replace(/^ /, ''));
    } else if (line === '') {
      if (dataLines.length > 0) {
        const payload = dataLines.join('\n').trim();
        dataLines = [];
        if (payload.length > 0) {
          messages.push(JSON.parse(payload));
        }
      }
    }
  }
  if (dataLines.length > 0) {
    const payload = dataLines.join('\n').trim();
    if (payload.length > 0) {
      messages.push(JSON.parse(payload));
    }
  }
  const match =
    expectedId !== undefined
      ? messages.find((m) => m.id === expectedId)
      : messages[0];
  assert.ok(
    match,
    `Expected JSON-RPC response (id=${expectedId}) in MCP SSE payload:\n${body}`,
  );
  return match;
}

function createMcpHttpClient(mcpUrl, token) {
  let sessionId = null;
  let nextId = 1;

  const baseHeaders = () => ({
    Accept: 'application/json, text/event-stream',
    'Content-Type': 'application/json',
    Authorization: `Bearer ${token}`,
    ...(sessionId
      ? {
          'Mcp-Session-Id': sessionId,
          'MCP-Protocol-Version': '2025-03-26',
        }
      : {}),
  });

  return {
    get sessionId() {
      return sessionId;
    },
    async initialize(clientName) {
      const id = nextId++;
      const res = await fetch(mcpUrl, {
        method: 'POST',
        headers: baseHeaders(),
        body: JSON.stringify({
          jsonrpc: '2.0',
          id,
          method: 'initialize',
          params: {
            protocolVersion: '2025-03-26',
            capabilities: {},
            clientInfo: { name: clientName, version: '0.1.0' },
          },
        }),
      });
      assert.equal(res.status, 200, `initialize must return 200 (got ${res.status})`);
      sessionId = res.headers.get('mcp-session-id');
      assert.ok(sessionId, 'initialize response must include Mcp-Session-Id header');
      const initMsg = await parseMcpHttpResponse(res, id);
      assert.equal(initMsg.error, undefined, `initialize JSON-RPC error: ${JSON.stringify(initMsg)}`);
      assert.equal(initMsg.result?.serverInfo?.name, 'fluxwright-mcp');

      const notifRes = await fetch(mcpUrl, {
        method: 'POST',
        headers: baseHeaders(),
        body: JSON.stringify({
          jsonrpc: '2.0',
          method: 'notifications/initialized',
        }),
      });
      await notifRes.body?.cancel();
      assert.equal(
        notifRes.status,
        202,
        `notifications/initialized must return 202 (got ${notifRes.status})`,
      );
      return initMsg.result;
    },
    async request(method, params = undefined) {
      const id = nextId++;
      const payload = { jsonrpc: '2.0', id, method };
      if (params !== undefined) payload.params = params;
      const res = await fetch(mcpUrl, {
        method: 'POST',
        headers: baseHeaders(),
        body: JSON.stringify(payload),
      });
      assert.equal(res.status, 200, `${method} must return 200 (got ${res.status})`);
      const msg = await parseMcpHttpResponse(res, id);
      assert.equal(msg.error, undefined, `${method} JSON-RPC error: ${JSON.stringify(msg)}`);
      return msg.result;
    },
    async callTool(name, args = {}) {
      const result = await this.request('tools/call', { name, arguments: args });
      assert.notEqual(
        result?.isError,
        true,
        `MCP tool ${name} returned isError=true: ${JSON.stringify(result)}`,
      );
      return result?.content?.[0]?.text ?? '';
    },
    async closeSession() {
      const res = await fetch(mcpUrl, {
        method: 'DELETE',
        headers: {
          Authorization: `Bearer ${token}`,
          'Mcp-Session-Id': sessionId,
          'MCP-Protocol-Version': '2025-03-26',
        },
      });
      await res.body?.cancel();
      assert.equal(res.status, 202, `DELETE /mcp must return 202 (got ${res.status})`);
    },
  };
}

async function runMcpStreamableHttpE2E(fixture) {
  console.log('[e2e] Running fluxwright-mcp Streamable HTTP E2E...');
  const mcpBin = resolveFluxwrightMcpBin();

  // 1. Fail-closed check: non-loopback --http without >=32-char token or --allow-unauthenticated
  const envWithoutToken = { ...process.env };
  delete envWithoutToken.FLUXWRIGHT_MCP_TOKEN;
  const noAuthNonLoopback = spawnSync(mcpBin, ['--http', '0.0.0.0:0'], {
    env: envWithoutToken,
    encoding: 'utf8',
    timeout: 10_000,
  });
  assert.notEqual(
    noAuthNonLoopback.status,
    0,
    'fluxwright-mcp --http 0.0.0.0:0 without token must fail closed',
  );
  assert.match(
    noAuthNonLoopback.stderr || '',
    /FLUXWRIGHT_MCP_TOKEN|--allow-unauthenticated/,
  );

  const shortTokenNonLoopback = spawnSync(mcpBin, ['--http', '0.0.0.0:0'], {
    env: { ...envWithoutToken, FLUXWRIGHT_MCP_TOKEN: 'short-token' },
    encoding: 'utf8',
    timeout: 10_000,
  });
  assert.notEqual(
    shortTokenNonLoopback.status,
    0,
    'fluxwright-mcp --http 0.0.0.0:0 with <32-char token must fail closed',
  );

  // 2. Start authenticated Streamable HTTP server on loopback
  const mcpToken = crypto.randomBytes(32).toString('hex');
  const mcpPort = await getFreePort();
  const mcpBaseUrl = `http://127.0.0.1:${mcpPort}`;
  const mcpUrl = `${mcpBaseUrl}/mcp`;
  const mcpLogs = [];

  const mcpProc = spawn(mcpBin, ['--http', `127.0.0.1:${mcpPort}`], {
    stdio: ['ignore', 'pipe', 'pipe'],
    env: {
      ...process.env,
      FLUXWRIGHT_MCP_TOKEN: mcpToken,
      FLUXWRIGHT_HEADLESS: '1',
      FLUXWRIGHT_NO_SANDBOX: process.env.FLUXWRIGHT_NO_SANDBOX || '1',
    },
  });
  const captureMcp = (chunk) => {
    mcpLogs.push(chunk.toString('utf8'));
    if (mcpLogs.length > 200) mcpLogs.shift();
  };
  mcpProc.stdout.on('data', captureMcp);
  mcpProc.stderr.on('data', captureMcp);

  try {
    await waitForHealth(mcpBaseUrl, mcpProc, mcpLogs);

    // Verify GET /healthz is unauthenticated and returns expected JSON
    const healthRes = await fetch(`${mcpBaseUrl}/healthz`);
    assert.equal(healthRes.status, 200);
    assert.deepEqual(await healthRes.json(), {
      status: 'ok',
      transport: 'streamable-http',
    });

    // Verify /mcp auth middleware rejects missing and invalid bearer tokens
    const initPayload = JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method: 'initialize',
      params: {
        protocolVersion: '2025-03-26',
        capabilities: {},
        clientInfo: { name: 'unauth-probe', version: '0.1.0' },
      },
    });
    const mcpHeaders = {
      Accept: 'application/json, text/event-stream',
      'Content-Type': 'application/json',
    };
    const noTokenRes = await fetch(mcpUrl, {
      method: 'POST',
      headers: mcpHeaders,
      body: initPayload,
    });
    await noTokenRes.body?.cancel();
    assert.equal(noTokenRes.status, 401, 'POST /mcp without Bearer token must return 401');

    const badTokenRes = await fetch(mcpUrl, {
      method: 'POST',
      headers: { ...mcpHeaders, Authorization: `Bearer wrong-${mcpToken.slice(6)}` },
      body: initPayload,
    });
    await badTokenRes.body?.cancel();
    assert.equal(badTokenRes.status, 401, 'POST /mcp with wrong Bearer token must return 401');

    // 3. Real HTTP MCP session A: initialize -> tools/list (22 tools) -> open -> snapshot
    const clientA = createMcpHttpClient(mcpUrl, mcpToken);
    const clientB = createMcpHttpClient(mcpUrl, mcpToken);

    await clientA.initialize('e2e-http-a');
    await clientB.initialize('e2e-http-b');
    assert.notEqual(clientA.sessionId, clientB.sessionId, 'sessions must receive distinct IDs');

    const listed = await clientA.request('tools/list');
    const expectedTools = [
      'open',
      'goto',
      'title',
      'content',
      'snapshot',
      'markdown',
      'click',
      'hover',
      'fill',
      'press',
      'select',
      'scroll',
      'wait_for',
      'back',
      'forward',
      'tabs',
      'console',
      'network',
      'export_api_calls',
      'evaluate',
      'screenshot',
      'close',
    ];
    const actualToolNames = (listed?.tools || []).map((t) => t.name).sort();
    assert.equal(actualToolNames.length, 22, `expected 22 MCP tools, got ${actualToolNames.length}`);
    assert.deepEqual(actualToolNames, [...expectedTools].sort());

    const openOut = await clientA.callTool('open', { url: `${fixture.url}/` });
    assert.match(openOut, /E2E Fixture/);

    const snapOut = await clientA.callTool('snapshot', {});
    assert.match(snapOut, /Fluxwright E2E Fixture/);
    assert.match(snapOut, /^rev: \d+/m);
    assert.match(snapOut, /\[ref=e\d+\]/);

    // 4. Multi-session context isolation: cookies, localStorage, and tabs stay isolated per session
    await clientA.callTool('evaluate', {
      expression:
        "document.cookie = 'mcp_tenant=alpha; Path=/'; localStorage.setItem('mcp_key', 'alpha-secret'); 'ok'",
    });
    const tabNewOut = await clientA.callTool('tabs', {
      action: 'new',
      url: `${fixture.url}/hitl`,
    });
    assert.match(tabNewOut, /opened tab 1/);

    await clientB.callTool('open', { url: `${fixture.url}/` });
    const rawBStateBefore = await clientB.callTool('evaluate', {
      expression:
        "JSON.stringify({ cookie: document.cookie, ls: localStorage.getItem('mcp_key') })",
    });
    const bStateBefore = JSON.parse(JSON.parse(rawBStateBefore));
    assert.doesNotMatch(bStateBefore.cookie, /mcp_tenant=alpha/);
    assert.equal(bStateBefore.ls, null);

    await clientB.callTool('evaluate', {
      expression:
        "document.cookie = 'mcp_tenant=beta; Path=/'; localStorage.setItem('mcp_key', 'beta-secret'); 'ok'",
    });
    const bTabs = await clientB.callTool('tabs', { action: 'list' });
    assert.match(bTabs, /\[0\]\*/);
    assert.doesNotMatch(bTabs, /\[1\]/);

    await clientA.callTool('tabs', { action: 'switch', index: 0 });
    const rawAStateAfter = await clientA.callTool('evaluate', {
      expression:
        "JSON.stringify({ cookie: document.cookie, ls: localStorage.getItem('mcp_key') })",
    });
    const aStateAfter = JSON.parse(JSON.parse(rawAStateAfter));
    assert.match(aStateAfter.cookie, /mcp_tenant=alpha/);
    assert.doesNotMatch(aStateAfter.cookie, /mcp_tenant=beta/);
    assert.equal(aStateAfter.ls, 'alpha-secret');

    // 5. Closing Session A invalidates Session A while keeping Session B operational
    await clientA.closeSession();
    const staleRes = await fetch(mcpUrl, {
      method: 'POST',
      headers: {
        ...mcpHeaders,
        Authorization: `Bearer ${mcpToken}`,
        'Mcp-Session-Id': clientA.sessionId,
        'MCP-Protocol-Version': '2025-03-26',
      },
      body: JSON.stringify({ jsonrpc: '2.0', id: 99, method: 'tools/list' }),
    });
    await staleRes.body?.cancel();
    assert.equal(staleRes.status, 404, 'closed MCP session must return 404');

    const titleB = await clientB.callTool('title', {});
    assert.equal(titleB, 'E2E Fixture');
    await clientB.closeSession();

    await stopChild(mcpProc);
    if (process.platform !== 'win32') {
      assert.equal(mcpProc.exitCode, 0, 'fluxwright-mcp must exit cleanly with code 0 on SIGTERM');
    }
    console.log('[e2e] fluxwright-mcp Streamable HTTP E2E passed.');
  } catch (err) {
    console.error('[e2e] MCP server logs:\n' + mcpLogs.join(''));
    throw err;
  } finally {
    await stopChild(mcpProc);
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
    await runCdpGatewayE2E(baseUrl, port, token, fixture);
    await runDashboardHitlE2E(baseUrl, port, token, fixture.url, chromePath);
    await runSiteE2E(chromePath);
    await runMcpStreamableHttpE2E(fixture);
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
