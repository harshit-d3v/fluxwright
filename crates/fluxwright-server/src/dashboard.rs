//! Live fleet dashboard UI and Server-Sent Events (SSE) streaming.
//!
//! Provides a real-time web dashboard for monitoring browser fleet metrics,
//! active and queued jobs, memory utilization, and OpenTelemetry trace IDs.
//! Meets WCAG AA accessibility standards and Modern Web Guidance.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use futures_util::stream::Stream;

use crate::Inner;

/// Serves the standalone HTML/CSS/JS fleet dashboard.
pub async fn dashboard_html() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

/// Server-Sent Events handler streaming live fleet metrics and job updates every second, until
/// the server shuts down: a graceful HTTP shutdown waits for every open response, so a stream
/// that never ended kept `fluxwright serve` from exiting while a dashboard was open.
pub(crate) async fn dashboard_events(
    State(inner): State<Arc<Inner>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = futures_util::stream::unfold((inner, true), |(inner, first)| async move {
        // The wait comes first, so each snapshot is fresh when it is sent.
        if !first {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
        if !inner.registry.lock().unwrap().accepting {
            return None;
        }
        let snapshot = inner.dashboard_snapshot().await;
        let json = serde_json::to_string(&snapshot).unwrap_or_default();
        Some((
            Ok::<_, Infallible>(Event::default().event("snapshot").data(json)),
            (inner, false),
        ))
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Fluxwright Fleet Dashboard</title>
  <style>
    :root {
      color-scheme: light dark;
      --bg: #0f172a;
      --card-bg: #1e293b;
      --border: #334155;
      --text: #f8fafc;
      --text-muted: #94a3b8;
      --primary: #38bdf8;
      --success: #4ade80;
      --warning: #fbbf24;
      --danger: #f87171;
      --font: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
      --mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    }
    @media (prefers-color-scheme: light) {
      :root {
        --bg: #f8fafc;
        --card-bg: #ffffff;
        --border: #e2e8f0;
        --text: #0f172a;
        --text-muted: #64748b;
        --primary: #0284c7;
        --success: #16a34a;
        --warning: #d97706;
        --danger: #dc2626;
      }
    }
    *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
    body {
      font-family: var(--font);
      background-color: var(--bg);
      color: var(--text);
      line-height: 1.5;
      padding: 1.5rem;
    }
    .skip-link {
      position: absolute;
      top: -40px;
      left: 1rem;
      background: var(--primary);
      color: #000;
      padding: 0.5rem 1rem;
      font-weight: 600;
      z-index: 100;
      text-decoration: none;
      border-radius: 4px;
      transition: top 0.1s;
    }
    .skip-link:focus { top: 1rem; outline: 3px solid #fff; }
    header {
      display: flex;
      flex-wrap: wrap;
      justify-content: space-between;
      align-items: center;
      gap: 1rem;
      margin-bottom: 2rem;
      padding-bottom: 1rem;
      border-bottom: 1px solid var(--border);
    }
    .brand { display: flex; align-items: center; gap: 0.75rem; }
    .brand h1 { font-size: 1.5rem; font-weight: 700; letter-spacing: -0.025em; }
    .status-pill {
      display: inline-flex;
      align-items: center;
      gap: 0.5rem;
      font-size: 0.875rem;
      padding: 0.25rem 0.75rem;
      border-radius: 9999px;
      background: var(--card-bg);
      border: 1px solid var(--border);
    }
    .status-dot {
      width: 0.625rem;
      height: 0.625rem;
      border-radius: 50%;
      background: var(--warning);
    }
    .status-dot.connected { background: var(--success); }
    .status-dot.disconnected { background: var(--danger); }
    .controls { display: flex; gap: 0.75rem; align-items: center; }
    button, input {
      font-family: inherit;
      font-size: 0.875rem;
      padding: 0.5rem 0.875rem;
      border-radius: 6px;
      border: 1px solid var(--border);
      background: var(--card-bg);
      color: var(--text);
    }
    button {
      cursor: pointer;
      font-weight: 500;
      transition: background-color 0.15s, border-color 0.15s;
    }
    button:hover { border-color: var(--primary); }
    button:focus-visible, input:focus-visible {
      outline: 2px solid var(--primary);
      outline-offset: 2px;
    }
    .grid-kpi {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
      gap: 1rem;
      margin-bottom: 1.5rem;
    }
    .card {
      background: var(--card-bg);
      border: 1px solid var(--border);
      border-radius: 8px;
      padding: 1.25rem;
    }
    .card-label {
      font-size: 0.8125rem;
      color: var(--text-muted);
      font-weight: 500;
      margin-bottom: 0.375rem;
    }
    .card-value {
      font-size: 1.75rem;
      font-weight: 700;
      font-variant-numeric: tabular-nums;
    }
    .card-sub {
      font-size: 0.75rem;
      color: var(--text-muted);
      margin-top: 0.25rem;
    }
    .section-title {
      font-size: 1.125rem;
      font-weight: 600;
      margin: 1.5rem 0 0.75rem;
    }
    .table-container {
      overflow-x: auto;
      background: var(--card-bg);
      border: 1px solid var(--border);
      border-radius: 8px;
    }
    table {
      width: 100%;
      border-collapse: collapse;
      text-align: left;
      font-size: 0.875rem;
    }
    caption {
      text-align: left;
      padding: 1rem 1.25rem 0.5rem;
      font-weight: 600;
      font-size: 1rem;
    }
    th, td {
      padding: 0.75rem 1.25rem;
      border-top: 1px solid var(--border);
      vertical-align: middle;
    }
    th {
      color: var(--text-muted);
      font-weight: 600;
      font-size: 0.75rem;
      text-transform: uppercase;
      letter-spacing: 0.05em;
    }
    .mono { font-family: var(--mono); font-size: 0.8125rem; }
    .badge {
      display: inline-block;
      padding: 0.125rem 0.5rem;
      border-radius: 4px;
      font-size: 0.75rem;
      font-weight: 600;
      text-transform: uppercase;
    }
    .badge-queued { background: #3b82f633; color: #60a5fa; }
    .badge-running { background: #f59e0b33; color: #fbbf24; }
    .badge-succeeded { background: #22c55e33; color: #4ade80; }
    .badge-failed { background: #ef444433; color: #f87171; }
    .badge-cancelled { background: #6b728033; color: #9ca3af; }
    .badge-online { background: #22c55e33; color: #4ade80; }
    .badge-draining { background: #f59e0b33; color: #fbbf24; }
    .badge-stale { background: #6b728033; color: #9ca3af; }
    .btn-sm { padding: 0.25rem 0.625rem; font-size: 0.75rem; }
    .meter-bar {
      height: 8px;
      background: var(--border);
      border-radius: 4px;
      overflow: hidden;
      margin-top: 0.5rem;
    }
    .meter-fill {
      height: 100%;
      background: var(--primary);
      transition: width 0.3s;
    }
    .visually-hidden {
      position: absolute;
      width: 1px;
      height: 1px;
      padding: 0;
      margin: -1px;
      overflow: hidden;
      clip: rect(0, 0, 0, 0);
      white-space: nowrap;
      border: 0;
    }
    #auth-dialog {
      margin: auto;
      background: var(--card-bg);
      color: var(--text);
      border: 1px solid var(--border);
      border-radius: 8px;
      padding: 1.5rem;
      max-width: 420px;
      width: 90%;
    }
    #auth-dialog::backdrop { background: rgba(0, 0, 0, 0.6); }
    #auth-dialog form { display: flex; flex-direction: column; gap: 1rem; }
  </style>
</head>
<body>
  <a href="#content" class="skip-link">Skip to main content</a>

  <header>
    <div class="brand">
      <h1>Fluxwright Fleet Dashboard</h1>
      <div class="status-pill" id="conn-pill">
        <span class="status-dot" id="conn-dot"></span>
        <span id="conn-label">Connecting...</span>
      </div>
    </div>
    <div class="controls">
      <button type="button" id="btn-auth">Set Token</button>
      <button type="button" id="btn-refresh">Refresh</button>
    </div>
  </header>

  <main id="content">
    <section aria-labelledby="kpi-heading">
      <h2 id="kpi-heading" class="visually-hidden">Fleet Key Performance Indicators</h2>
      <div class="grid-kpi">
        <div class="card">
          <div class="card-label">Active Leases</div>
          <div class="card-value" id="val-active-leases">0</div>
          <div class="card-sub" id="val-contexts">0 contexts open</div>
        </div>
        <div class="card">
          <div class="card-label">Queued Requests</div>
          <div class="card-value" id="val-queued">0</div>
          <div class="card-sub" id="val-pool-waiting">in engine pool</div>
        </div>
        <div class="card">
          <div class="card-label">Browsers Online</div>
          <div class="card-value" id="val-browsers">0</div>
          <div class="card-sub" id="val-pages">0 pages allocated</div>
        </div>
        <div class="card">
          <div class="card-label">Browser Reuse Rate</div>
          <div class="card-value" id="val-reuse-rate">0%</div>
          <div class="card-sub" id="val-recycle-crash">0 recycles &middot; 0 crashes</div>
        </div>
        <div class="card">
          <div class="card-label">Memory Usage</div>
          <div class="card-value" id="val-mem-total">0 MB</div>
          <div class="meter-bar"><div class="meter-fill" id="meter-mem" style="width: 0%"></div></div>
          <div class="card-sub" id="val-mem-sub">0 MB engine &middot; 0 MB tree</div>
        </div>
        <div class="card">
          <div class="card-label">HTTP Jobs Completed</div>
          <div class="card-value" id="val-jobs-total">0</div>
          <div class="card-sub" id="val-jobs-breakdown">0 ok &middot; 0 err &middot; 0 cancelled</div>
        </div>
      </div>
    </section>

    <section id="workers-section" hidden aria-labelledby="workers-heading" style="margin-bottom: 1.5rem;">
      <div class="table-container">
        <table>
          <caption id="workers-heading">Cluster Workers</caption>
          <thead>
            <tr>
              <th scope="col">Worker</th>
              <th scope="col">Status</th>
              <th scope="col">Active / Max</th>
              <th scope="col">Jobs (ok / err / exp)</th>
              <th scope="col">Last Seen</th>
              <th scope="col">Action</th>
            </tr>
          </thead>
          <tbody id="workers-body">
            <tr><td colspan="6" style="text-align: center; color: var(--text-muted); padding: 1.5rem;">No workers registered yet.</td></tr>
          </tbody>
        </table>
      </div>
    </section>

    <section aria-labelledby="jobs-heading">
      <div class="table-container">
        <table>
          <caption id="jobs-heading">Recent HTTP Jobs</caption>
          <thead>
            <tr>
              <th scope="col">Job ID</th>
              <th scope="col">Status</th>
              <th scope="col">Worker</th>
              <th scope="col">Trace ID (W3C)</th>
              <th scope="col">Attempts</th>
              <th scope="col">Step Index</th>
              <th scope="col">Duration</th>
              <th scope="col">Error / Summary</th>
            </tr>
          </thead>
          <tbody id="jobs-body">
            <tr><td colspan="8" style="text-align: center; color: var(--text-muted); padding: 2rem;">Waiting for fleet job data...</td></tr>
          </tbody>
        </table>
      </div>
    </section>
  </main>

  <div id="announcer" aria-live="polite" class="visually-hidden"></div>

  <dialog id="auth-dialog">
    <form method="dialog" id="auth-form">
      <h2>Server Authentication</h2>
      <p style="font-size: 0.875rem; color: var(--text-muted);">
        Enter the <code class="mono">FLUXWRIGHT_SERVER_TOKEN</code> configured on your server to view live data.
      </p>
      <label for="token-input" style="font-size: 0.875rem; font-weight: 500;">Bearer Token:</label>
      <input type="password" id="token-input" autocomplete="current-password" required placeholder="32-256 character token">
      <div style="display: flex; justify-content: flex-end; gap: 0.5rem; margin-top: 0.5rem;">
        <button type="button" id="btn-cancel-auth">Cancel</button>
        <button type="submit" style="background: var(--primary); color: #000;">Save Token</button>
      </div>
    </form>
  </dialog>

  <script>
    (function () {
      // Never from the URL: a token there lands in proxy logs and browser history.
      let token = sessionStorage.getItem('fluxwright_token') || '';
      let stream = null; // AbortController of the open event stream
      let retryTimer = null;
      const announcer = document.getElementById('announcer');
      const dialog = document.getElementById('auth-dialog');
      const tokenInput = document.getElementById('token-input');

      function announce(msg) {
        if (announcer) announcer.textContent = msg;
      }

      function updateConnectionStatus(status) {
        const dot = document.getElementById('conn-dot');
        const label = document.getElementById('conn-label');
        dot.className = 'status-dot ' + status;
        if (status === 'connected') label.textContent = 'Connected (live)';
        else if (status === 'disconnected') label.textContent = 'Disconnected';
        else label.textContent = 'Connecting...';
      }

      function formatBytes(bytes) {
        if (!bytes) return '0 MB';
        return (bytes / (1024 * 1024)).toFixed(1) + ' MB';
      }

      function askForToken() {
        if (!dialog.open) dialog.showModal();
      }

      // One server-sent event block: "event:" and "data:" lines.
      function handleEvent(block) {
        let name = 'message';
        const data = [];
        for (const line of block.split('\n')) {
          if (line.startsWith('event:')) name = line.slice(6).trim();
          else if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
        }
        if (name !== 'snapshot' || data.length === 0) return;
        updateConnectionStatus('connected');
        try {
          renderDashboard(JSON.parse(data.join('\n')));
        } catch (err) {
          console.error('Failed to parse snapshot JSON', err);
        }
      }

      // fetch, not EventSource: EventSource cannot send the Authorization header, and the
      // server does not take the token any other way.
      async function connectSSE() {
        if (stream) stream.abort();
        clearTimeout(retryTimer);
        const controller = new AbortController();
        stream = controller;
        updateConnectionStatus('connecting');
        if (!token) {
          updateConnectionStatus('disconnected');
          askForToken();
          return;
        }
        try {
          const res = await fetch('/v1/events', {
            headers: { Authorization: 'Bearer ' + token },
            cache: 'no-store',
            signal: controller.signal,
          });
          if (res.status === 401) {
            updateConnectionStatus('disconnected');
            askForToken();
            return;
          }
          if (!res.ok || !res.body) throw new Error('HTTP ' + res.status);
          const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
          let buffer = '';
          for (;;) {
            const { value, done } = await reader.read();
            if (done) break;
            buffer += value.replace(/\r\n?/g, '\n');
            let end;
            while ((end = buffer.indexOf('\n\n')) !== -1) {
              handleEvent(buffer.slice(0, end));
              buffer = buffer.slice(end + 2);
            }
          }
        } catch (err) {
          if (controller.signal.aborted) return;
        }
        // The stream ended (server restart, network): try again shortly, as EventSource did.
        if (stream === controller) {
          updateConnectionStatus('disconnected');
          retryTimer = setTimeout(connectSSE, 3000);
        }
      }

      function renderDashboard(data) {
        const fleet = data.fleet || {};
        const srv = data.server || {};

        document.getElementById('val-active-leases').textContent = fleet.active_leases != null ? fleet.active_leases : 0;
        document.getElementById('val-contexts').textContent = (fleet.contexts != null ? fleet.contexts : 0) + ' contexts open';
        document.getElementById('val-queued').textContent = srv.queued_jobs || 0;
        document.getElementById('val-pool-waiting').textContent = (fleet.queued_requests || 0) + ' in engine pool';
        document.getElementById('val-browsers').textContent = fleet.browsers != null ? fleet.browsers : 0;
        document.getElementById('val-pages').textContent = (fleet.pages != null ? fleet.pages : 0) + ' pages allocated';

        const reuse = ((fleet.browser_reuse_rate || 0) * 100).toFixed(1) + '%';
        document.getElementById('val-reuse-rate').textContent = reuse;
        document.getElementById('val-recycle-crash').textContent =
          (fleet.recycle_count || 0) + ' recycles \u00b7 ' + (fleet.crash_count || 0) + ' crashes';

        const engineRss = fleet.engine_rss_bytes || 0;
        const treeRss = fleet.browser_tree_rss_bytes || 0;
        const totalMem = engineRss + treeRss;
        document.getElementById('val-mem-total').textContent = formatBytes(totalMem);
        document.getElementById('val-mem-sub').textContent =
          formatBytes(engineRss) + ' engine \u00b7 ' + formatBytes(treeRss) + ' tree';

        const totalJobs = (srv.succeeded_jobs || 0) + (srv.failed_jobs || 0) + (srv.cancelled_jobs || 0);
        document.getElementById('val-jobs-total').textContent = totalJobs;
        document.getElementById('val-jobs-breakdown').textContent =
          (srv.succeeded_jobs || 0) + ' ok \u00b7 ' + (srv.failed_jobs || 0) + ' err \u00b7 ' + (srv.cancelled_jobs || 0) + ' cancelled';

        renderWorkers(data);

        const tbody = document.getElementById('jobs-body');
        const jobs = data.jobs || [];
        if (jobs.length === 0) {
          tbody.innerHTML = '<tr><td colspan="8" style="text-align: center; color: var(--text-muted); padding: 2rem;">No recent jobs in registry.</td></tr>';
          return;
        }

        tbody.innerHTML = jobs.map(function (j) {
          const statusClass = 'badge-' + (j.status || 'queued');
          const workerCell = j.worker ? '<span class="mono">' + escapeHtml(j.worker) + '</span>' : '&mdash;';
          const traceId = j.trace_id ? '<span class="mono">' + escapeHtml(j.trace_id.substring(0, 16)) + '&hellip;</span>' : '&mdash;';
          const step = j.step_index != null ? j.step_index : '&mdash;';
          let dur = '&mdash;';
          if (j.started_at && j.finished_at) {
            dur = (j.finished_at - j.started_at) + ' ms';
          } else if (j.started_at) {
            dur = (Date.now() - j.started_at) + ' ms (running)';
          }
          const err = j.error ? '<span style="color: var(--danger);">' + escapeHtml(j.error) + '</span>' : '&mdash;';

          return '<tr>' +
            '<td class="mono">' + escapeHtml(j.id) + '</td>' +
            '<td><span class="badge ' + statusClass + '">' + escapeHtml(j.status) + '</span></td>' +
            '<td>' + workerCell + '</td>' +
            '<td>' + traceId + '</td>' +
            '<td>' + (j.attempts || 0) + '</td>' +
            '<td>' + step + '</td>' +
            '<td>' + dur + '</td>' +
            '<td>' + err + '</td>' +
            '</tr>';
        }).join('');
      }

      function renderWorkers(data) {
        const section = document.getElementById('workers-section');
        const body = document.getElementById('workers-body');
        const workers = Array.isArray(data.workers) ? data.workers : [];
        const show = data.mode === 'coordinator' || workers.length > 0;
        section.hidden = !show;
        if (!show) return;

        body.textContent = '';
        if (workers.length === 0) {
          const tr = document.createElement('tr');
          const td = document.createElement('td');
          td.colSpan = 6;
          td.style.textAlign = 'center';
          td.style.color = 'var(--text-muted)';
          td.style.padding = '1.5rem';
          td.textContent = 'No workers registered yet.';
          tr.appendChild(td);
          body.appendChild(tr);
          return;
        }

        const nowMs = Date.now();
        for (const w of workers) {
          const tr = document.createElement('tr');

          const tdId = document.createElement('td');
          tdId.className = 'mono';
          tdId.textContent = String(w.worker || '');
          tr.appendChild(tdId);

          const tdStatus = document.createElement('td');
          const badge = document.createElement('span');
          const st = w.status === 'online' || w.status === 'draining' || w.status === 'stale' ? w.status : 'stale';
          badge.className = 'badge badge-' + st;
          badge.textContent = st;
          tdStatus.appendChild(badge);
          tr.appendChild(tdStatus);

          const tdSlots = document.createElement('td');
          tdSlots.textContent = (w.active_jobs || 0) + ' / ' + (w.max_jobs || 0);
          tr.appendChild(tdSlots);

          const tdCounts = document.createElement('td');
          tdCounts.textContent = (w.succeeded_jobs || 0) + ' / ' + (w.failed_jobs || 0) + ' / ' + (w.expired_leases || 0);
          tr.appendChild(tdCounts);

          const tdSeen = document.createElement('td');
          const ageSec = Math.max(0, Math.round((nowMs - (w.last_seen_at || nowMs)) / 1000));
          tdSeen.textContent = ageSec + 's ago';
          tr.appendChild(tdSeen);

          const tdAction = document.createElement('td');
          if (!w.draining) {
            const btn = document.createElement('button');
            btn.type = 'button';
            btn.className = 'btn-sm';
            btn.dataset.workerId = String(w.worker || '');
            btn.textContent = 'Drain';
            tdAction.appendChild(btn);
          } else {
            tdAction.textContent = 'Draining';
          }
          tr.appendChild(tdAction);

          body.appendChild(tr);
        }
      }

      document.getElementById('workers-body').addEventListener('click', async function (e) {
        const btn = e.target && e.target.closest ? e.target.closest('button[data-worker-id]') : null;
        if (!btn) return;
        const workerId = btn.dataset.workerId;
        if (!workerId || !token) return;
        btn.disabled = true;
        try {
          const res = await fetch('/v1/workers/' + encodeURIComponent(workerId) + '/drain', {
            method: 'POST',
            headers: { Authorization: 'Bearer ' + token },
          });
          if (res.ok) {
            announce('Worker ' + workerId + ' draining');
          } else {
            btn.disabled = false;
            announce('Failed to drain worker ' + workerId);
          }
        } catch (err) {
          btn.disabled = false;
        }
      });

      function escapeHtml(s) {
        if (!s) return '';
        return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
      }

      document.getElementById('btn-auth').addEventListener('click', function () {
        tokenInput.value = token;
        dialog.showModal();
      });

      document.getElementById('btn-cancel-auth').addEventListener('click', function () {
        dialog.close();
      });

      document.getElementById('auth-form').addEventListener('submit', function (e) {
        e.preventDefault();
        token = tokenInput.value.trim();
        sessionStorage.setItem('fluxwright_token', token);
        dialog.close();
        announce('Token saved; reconnecting...');
        connectSSE();
      });

      document.getElementById('btn-refresh').addEventListener('click', function () {
        connectSSE();
      });

      connectSSE();
    })();
  </script>
</body>
</html>
"##;
