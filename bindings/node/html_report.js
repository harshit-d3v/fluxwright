'use strict'

const fs = require('node:fs/promises')
const path = require('node:path')

function escapeHtml(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}

function formatDuration(ms) {
  const n = Number(ms) || 0
  if (n < 1000) return `${Math.round(n)}ms`
  if (n < 60_000) return `${(n / 1000).toFixed(2)}s`
  const mins = Math.floor(n / 60_000)
  const secs = ((n % 60_000) / 1000).toFixed(1)
  return `${mins}m ${secs}s`
}

async function resolveAttachmentDataUri(att) {
  if (!att || !att.contentType || !att.contentType.startsWith('image/')) return null
  if (att.dataUri) return att.dataUri
  let buf = null
  if (Buffer.isBuffer(att.body)) {
    buf = att.body
  } else if (typeof att.body === 'string') {
    buf = Buffer.from(att.body, 'base64')
  } else if (att.path) {
    try {
      buf = await fs.readFile(att.path)
    } catch {
      return null
    }
  }
  if (!buf || !buf.length) return null
  return `data:${att.contentType};base64,${buf.toString('base64')}`
}

async function prepareReport(report) {
  const results = []
  for (const r of report.results ?? []) {
    const attachments = []
    for (const att of r.attachments ?? []) {
      const dataUri = await resolveAttachmentDataUri(att)
      attachments.push({ ...att, dataUri })
    }
    results.push({ ...r, attachments })
  }
  return { ...report, results }
}

function computeSummary(results) {
  const counts = { total: results.length, passed: 0, failed: 0, flaky: 0, skipped: 0 }
  for (const r of results) {
    if (r.status === 'passed') counts.passed += 1
    else if (r.status === 'failed' || r.status === 'timedOut') counts.failed += 1
    else if (r.status === 'flaky') counts.flaky += 1
    else if (r.status === 'skipped') counts.skipped += 1
  }
  return counts
}

function renderTestCard(result, index) {
  const status = result.status === 'timedOut' ? 'failed' : result.status
  const label = result.status === 'timedOut' ? 'TIMED OUT' : String(result.status || 'unknown').toUpperCase()
  const titlePath = Array.isArray(result.titlePath) && result.titlePath.length
    ? result.titlePath.join(' › ')
    : result.title || `Test #${index + 1}`
  const searchKey = `${titlePath} ${result.file || ''}`.toLowerCase()
  const openAttr = status === 'failed' ? ' open' : ''

  let bodyHtml = ''
  if (result.error) {
    const msg = escapeHtml(result.error.message || String(result.error))
    const stack = result.error.stack ? `\n\n${escapeHtml(result.error.stack)}` : ''
    bodyHtml += `<div class="error-box"><strong>Error</strong><pre>${msg}${stack}</pre></div>`
  }

  const atts = result.attachments ?? []
  if (atts.length > 0) {
    const attParts = atts.map((att) => {
      if (att.dataUri) {
        return (
          `<div class="attachment">` +
          `<div class="att-label">${escapeHtml(att.name || 'screenshot')} (${escapeHtml(att.contentType)})</div>` +
          `<img src="${escapeHtml(att.dataUri)}" alt="${escapeHtml(att.name || 'Failure screenshot')}" />` +
          `</div>`
        )
      }
      const pathText = att.path ? `<code>${escapeHtml(att.path)}</code>` : ''
      return (
        `<div class="attachment att-file">` +
        `<span class="att-label">${escapeHtml(att.name || 'attachment')} (${escapeHtml(att.contentType || 'application/octet-stream')})</span> ` +
        pathText +
        `</div>`
      )
    })
    bodyHtml += `<div class="attachments">${attParts.join('')}</div>`
  }

  if (!bodyHtml) {
    bodyHtml = `<div class="muted">No errors or attachments.</div>`
  }

  const retryPill = result.retries > 0 ? `<span class="pill pill-retry">retries: ${result.retries}</span>` : ''

  return (
    `<details class="test-card status-${escapeHtml(status)}" data-status="${escapeHtml(status)}" data-search="${escapeHtml(searchKey)}"${openAttr}>` +
    `<summary>` +
    `<span class="badge badge-${escapeHtml(status)}">${escapeHtml(label)}</span>` +
    `<span class="test-title">${escapeHtml(titlePath)}</span>` +
    `${retryPill}` +
    `<span class="test-duration">${escapeHtml(formatDuration(result.durationMs))}</span>` +
    `</summary>` +
    `<div class="test-body">${bodyHtml}</div>` +
    `</details>`
  )
}

function generateHtmlReport(report) {
  const results = report.results ?? []
  const summary = computeSummary(results)
  const durationText = formatDuration(report.durationMs ?? 0)
  const shardText = report.shard ? `<span class="meta-pill">Shard ${escapeHtml(report.shard)}</span>` : ''
  const workersText = report.workers ? `<span class="meta-pill">Workers: ${escapeHtml(report.workers)}</span>` : ''
  const timestamp = escapeHtml(report.startedAt || new Date().toISOString())
  const cardsHtml = results.map((r, i) => renderTestCard(r, i)).join('\n')

  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>Fluxwright Test Report</title>
<style>
  :root {
    color-scheme: light dark;
    --bg: #0f172a;
    --surface: #1e293b;
    --surface-2: #0b1220;
    --border: #334155;
    --text: #e2e8f0;
    --muted: #94a3b8;
    --pass: #22c55e;
    --fail: #ef4444;
    --flaky: #f59e0b;
    --skip: #64748b;
    --accent: #38bdf8;
  }
  @media (prefers-color-scheme: light) {
    :root {
      --bg: #f8fafc;
      --surface: #ffffff;
      --surface-2: #f1f5f9;
      --border: #e2e8f0;
      --text: #0f172a;
      --muted: #64748b;
    }
  }
  * { box-sizing: border-box; }
  body {
    margin: 0;
    padding: 24px;
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    background: var(--bg);
    color: var(--text);
    line-height: 1.5;
  }
  .container { max-width: 1100px; margin: 0 auto; }
  header {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 20px;
  }
  h1 { margin: 0; font-size: 1.5rem; letter-spacing: -0.02em; }
  .meta { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; font-size: 0.85rem; color: var(--muted); }
  .meta-pill, .pill {
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--surface);
    font-size: 0.78rem;
  }
  .summary-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
    gap: 12px;
    margin-bottom: 20px;
  }
  .stat {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 12px 16px;
  }
  .stat-label { font-size: 0.78rem; color: var(--muted); text-transform: uppercase; letter-spacing: 0.04em; }
  .stat-val { font-size: 1.6rem; font-weight: 700; margin-top: 2px; }
  .stat-pass .stat-val { color: var(--pass); }
  .stat-fail .stat-val { color: var(--fail); }
  .stat-flaky .stat-val { color: var(--flaky); }
  .stat-skip .stat-val { color: var(--skip); }
  .toolbar {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    align-items: center;
    margin-bottom: 16px;
  }
  .filter-btn {
    border: 1px solid var(--border);
    background: var(--surface);
    color: var(--text);
    padding: 6px 12px;
    border-radius: 8px;
    cursor: pointer;
    font-size: 0.85rem;
  }
  .filter-btn.active { border-color: var(--accent); color: var(--accent); font-weight: 600; }
  .search-input {
    margin-left: auto;
    min-width: 220px;
    padding: 6px 12px;
    border-radius: 8px;
    border: 1px solid var(--border);
    background: var(--surface);
    color: var(--text);
    font-size: 0.9rem;
  }
  .test-list { display: flex; flex-direction: column; gap: 10px; }
  .test-card {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 10px;
    overflow: hidden;
  }
  .test-card.status-failed { border-left: 4px solid var(--fail); }
  .test-card.status-passed { border-left: 4px solid var(--pass); }
  .test-card.status-flaky { border-left: 4px solid var(--flaky); }
  .test-card.status-skipped { border-left: 4px solid var(--skip); }
  .test-card summary {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px 14px;
    cursor: pointer;
    list-style: none;
  }
  .test-card summary::-webkit-details-marker { display: none; }
  .badge {
    font-size: 0.72rem;
    font-weight: 700;
    padding: 2px 8px;
    border-radius: 6px;
    text-transform: uppercase;
    flex-shrink: 0;
  }
  .badge-passed { background: rgba(34, 197, 94, 0.16); color: var(--pass); }
  .badge-failed { background: rgba(239, 68, 68, 0.16); color: var(--fail); }
  .badge-flaky { background: rgba(245, 158, 11, 0.16); color: var(--flaky); }
  .badge-skipped { background: rgba(100, 116, 139, 0.16); color: var(--skip); }
  .test-title { font-weight: 500; word-break: break-word; flex: 1; }
  .test-duration { font-size: 0.82rem; color: var(--muted); flex-shrink: 0; font-variant-numeric: tabular-nums; }
  .test-body { border-top: 1px solid var(--border); padding: 14px; background: var(--surface-2); }
  .error-box pre {
    margin: 8px 0 0;
    padding: 12px;
    border-radius: 8px;
    background: rgba(239, 68, 68, 0.08);
    border: 1px solid rgba(239, 68, 68, 0.25);
    overflow-x: auto;
    font-size: 0.82rem;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .attachments { margin-top: 12px; display: flex; flex-direction: column; gap: 12px; }
  .att-label { font-size: 0.8rem; color: var(--muted); margin-bottom: 6px; }
  .attachment img {
    max-width: 100%;
    height: auto;
    border-radius: 8px;
    border: 1px solid var(--border);
    display: block;
  }
  .muted { color: var(--muted); font-size: 0.85rem; }
  .hidden { display: none !important; }
</style>
</head>
<body>
<div class="container">
  <header>
    <h1>Fluxwright Test Report</h1>
    <div class="meta">
      ${workersText}
      ${shardText}
      <span class="meta-pill">${timestamp}</span>
    </div>
  </header>
  <section class="summary-grid">
    <div class="stat"><div class="stat-label">Total</div><div class="stat-val" data-count="total">${summary.total}</div></div>
    <div class="stat stat-pass"><div class="stat-label">Passed</div><div class="stat-val" data-count="passed">${summary.passed}</div></div>
    <div class="stat stat-fail"><div class="stat-label">Failed</div><div class="stat-val" data-count="failed">${summary.failed}</div></div>
    <div class="stat stat-flaky"><div class="stat-label">Flaky</div><div class="stat-val" data-count="flaky">${summary.flaky}</div></div>
    <div class="stat stat-skip"><div class="stat-label">Skipped</div><div class="stat-val" data-count="skipped">${summary.skipped}</div></div>
    <div class="stat"><div class="stat-label">Duration</div><div class="stat-val">${escapeHtml(durationText)}</div></div>
  </section>
  <div class="toolbar">
    <button class="filter-btn active" data-filter="all">All (${summary.total})</button>
    <button class="filter-btn" data-filter="passed">Passed (${summary.passed})</button>
    <button class="filter-btn" data-filter="failed">Failed (${summary.failed})</button>
    <button class="filter-btn" data-filter="flaky">Flaky (${summary.flaky})</button>
    <button class="filter-btn" data-filter="skipped">Skipped (${summary.skipped})</button>
    <input class="search-input" type="search" placeholder="Filter tests..." aria-label="Filter tests" />
  </div>
  <main class="test-list">
    ${cardsHtml || '<div class="muted">No tests recorded.</div>'}
  </main>
</div>
<script>
(() => {
  const buttons = Array.from(document.querySelectorAll('.filter-btn'));
  const search = document.querySelector('.search-input');
  const cards = Array.from(document.querySelectorAll('.test-card'));
  let activeFilter = 'all';
  function apply() {
    const q = (search && search.value || '').trim().toLowerCase();
    for (const card of cards) {
      const statusOk = activeFilter === 'all' || card.dataset.status === activeFilter;
      const searchOk = !q || (card.dataset.search || '').includes(q);
      card.classList.toggle('hidden', !(statusOk && searchOk));
    }
  }
  for (const btn of buttons) {
    btn.addEventListener('click', () => {
      activeFilter = btn.dataset.filter || 'all';
      for (const b of buttons) b.classList.toggle('active', b === btn);
      apply();
    });
  }
  if (search) search.addEventListener('input', apply);
})();
</script>
</body>
</html>
`
}

async function writeHtmlReport(report, outputPath) {
  const resolved = path.resolve(outputPath)
  const prepared = await prepareReport(report)
  const html = generateHtmlReport(prepared)
  await fs.mkdir(path.dirname(resolved), { recursive: true })
  await fs.writeFile(resolved, html, 'utf8')
  return resolved
}

module.exports = {
  escapeHtml,
  formatDuration,
  generateHtmlReport,
  prepareReport,
  writeHtmlReport,
}
