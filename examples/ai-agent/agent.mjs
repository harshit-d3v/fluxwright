#!/usr/bin/env node
/**
 * Fluxwright AI Agent Starter
 *
 * Demonstrates two core agent integration patterns without external dependencies:
 * 1. Streamable HTTP MCP loop against `fluxwright-mcp --http`:
 *      initialize -> notifications/initialized -> tools/list -> open -> snapshot -> close
 * 2. Managed browser session allocation via `POST /v1/sessions` returning a signed
 *    `live_view_url` for Human-in-the-Loop (HITL) inspection and takeover.
 */

import { setTimeout as sleep } from 'node:timers/promises'

const MCP_URL = process.env.FLUXWRIGHT_MCP_URL ?? 'http://127.0.0.1:8090/mcp'
const MCP_TOKEN = process.env.FLUXWRIGHT_MCP_TOKEN?.trim()
const SERVER_URL = (process.env.FLUXWRIGHT_SERVER_URL ?? 'http://127.0.0.1:7331').replace(/\/+$/, '')
const SERVER_TOKEN = process.env.FLUXWRIGHT_SERVER_TOKEN?.trim()
const TARGET_URL = process.argv[2] ?? process.env.TARGET_URL ?? 'https://example.com'
const HITL_HOLD_SECS = Number(process.env.HITL_HOLD_SECS ?? 0)

function parseMcpResponse(contentType, bodyText) {
  if (!bodyText.trim()) return null
  if (contentType.includes('application/json')) {
    return JSON.parse(bodyText)
  }
  for (const line of bodyText.split(/\r?\n/)) {
    if (!line.startsWith('data:')) continue
    const data = line.slice(5).trim()
    if (data) return JSON.parse(data)
  }
  throw new Error(`Unable to parse MCP response (${contentType}): ${bodyText}`)
}

class StreamableMcpClient {
  #url
  #token
  #sessionId = null
  #nextId = 1

  constructor(url, token) {
    this.#url = url
    this.#token = token
  }

  #headers(includeJson = true) {
    return {
      Accept: 'application/json, text/event-stream',
      ...(includeJson ? { 'Content-Type': 'application/json' } : {}),
      ...(this.#token ? { Authorization: `Bearer ${this.#token}` } : {}),
      ...(this.#sessionId ? { 'mcp-session-id': this.#sessionId } : {}),
    }
  }

  async request(method, params) {
    const id = this.#nextId++
    const res = await fetch(this.#url, {
      method: 'POST',
      headers: this.#headers(true),
      body: JSON.stringify({
        jsonrpc: '2.0',
        id,
        method,
        ...(params !== undefined ? { params } : {}),
      }),
    })
    const sid = res.headers.get('mcp-session-id')
    if (sid) this.#sessionId = sid

    const text = await res.text()
    if (!res.ok) {
      throw new Error(`MCP ${method} failed (${res.status}): ${text}`)
    }
    const payload = parseMcpResponse(res.headers.get('content-type') ?? '', text)
    if (payload?.error) {
      throw new Error(`MCP ${method} error ${payload.error.code}: ${payload.error.message}`)
    }
    return payload?.result
  }

  async notify(method, params) {
    const res = await fetch(this.#url, {
      method: 'POST',
      headers: this.#headers(true),
      body: JSON.stringify({
        jsonrpc: '2.0',
        method,
        ...(params !== undefined ? { params } : {}),
      }),
    })
    if (!res.ok) {
      throw new Error(`MCP notification ${method} failed (${res.status}): ${await res.text()}`)
    }
  }

  async closeSession() {
    if (!this.#sessionId) return
    await fetch(this.#url, {
      method: 'DELETE',
      headers: this.#headers(false),
    }).catch(() => {})
    this.#sessionId = null
  }
}

async function runMcpDemo() {
  const client = new StreamableMcpClient(MCP_URL, MCP_TOKEN)
  try {
    console.log(`[mcp] Connecting to ${MCP_URL}`)
    const init = await client.request('initialize', {
      protocolVersion: '2025-03-26',
      capabilities: {},
      clientInfo: { name: 'fluxwright-ai-agent-starter', version: '1.0.0' },
    })
    console.log(`[mcp] Connected: ${init.serverInfo.name}@${init.serverInfo.version}`)

    await client.notify('notifications/initialized')

    const { tools } = await client.request('tools/list')
    console.log(`[mcp] Discovered ${tools.length} tools: ${tools.map((t) => t.name).join(', ')}`)

    const opened = await client.request('tools/call', {
      name: 'open',
      arguments: { url: TARGET_URL },
    })
    console.log(`[mcp] open -> ${opened.content?.[0]?.text ?? ''}`)

    const snap = await client.request('tools/call', {
      name: 'snapshot',
      arguments: { interactive_only: true },
    })
    const snapText = snap.content?.[0]?.text ?? ''
    console.log('[mcp] snapshot (interactive @eN refs):\n' + snapText.slice(0, 800))

    await client.request('tools/call', { name: 'close', arguments: {} })
  } finally {
    await client.closeSession()
  }
}

async function runHitlSessionDemo() {
  if (!SERVER_TOKEN) {
    console.log(
      '\n[hitl] Skipping /v1/sessions demo (set FLUXWRIGHT_SERVER_TOKEN to allocate a Live View session).',
    )
    return
  }

  const headers = {
    Authorization: `Bearer ${SERVER_TOKEN}`,
    'Content-Type': 'application/json',
  }

  console.log(`\n[hitl] Creating managed session on ${SERVER_URL}/v1/sessions`)
  const createRes = await fetch(`${SERVER_URL}/v1/sessions`, {
    method: 'POST',
    headers,
    body: JSON.stringify({
      ttl_secs: 300,
      keep_alive: true,
      live_view_mode: 'interactive',
      record_frames: true,
      viewport: { width: 1280, height: 720 },
    }),
  })
  if (!createRes.ok) {
    throw new Error(`POST /v1/sessions failed (${createRes.status}): ${await createRes.text()}`)
  }

  const session = await createRes.json()
  try {
    console.log(`[hitl] Session ID:    ${session.id}`)
    console.log(`[hitl] CDP WebSocket: ${session.cdp_ws_url}`)
    console.log(`[hitl] Live View URL: ${session.live_view_url}`)

    if (HITL_HOLD_SECS > 0) {
      console.log(`[hitl] Holding session open for ${HITL_HOLD_SECS}s for human inspection...`)
      await sleep(HITL_HOLD_SECS * 1000)
    }
  } finally {
    await fetch(`${SERVER_URL}/v1/sessions/${session.id}`, {
      method: 'DELETE',
      headers: { Authorization: `Bearer ${SERVER_TOKEN}` },
    }).catch(() => {})
    console.log(`[hitl] Released session ${session.id}`)
  }
}

async function main() {
  await runMcpDemo()
  await runHitlSessionDemo()
}

main().catch((err) => {
  console.error(`[ai-agent] Fatal: ${err.message}`)
  process.exit(1)
})
