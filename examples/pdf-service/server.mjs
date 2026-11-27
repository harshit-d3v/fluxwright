#!/usr/bin/env node
/**
 * Fluxwright PDF & Screenshot Microservice Starter
 *
 * Exposes:
 *   GET  /healthz
 *   POST /render/pdf        -> application/pdf
 *   POST /render/screenshot -> image/png | image/jpeg | image/webp
 *
 * Backed by `fluxwright serve` (`/v1/jobs` with `pdf` and `screenshot` steps).
 */

import http from 'node:http'
import { setTimeout as sleep } from 'node:timers/promises'

const HOST = process.env.HOST ?? '127.0.0.1'
const PORT = Number(process.env.PORT ?? 3000)
const SERVER_URL = (process.env.FLUXWRIGHT_SERVER_URL ?? 'http://127.0.0.1:7331').replace(/\/+$/, '')
const SERVER_TOKEN = process.env.FLUXWRIGHT_SERVER_TOKEN?.trim()
const POLL_INTERVAL_MS = Number(process.env.POLL_INTERVAL_MS ?? 250)
const MAX_BODY_BYTES = 64 * 1024

if (!SERVER_TOKEN) {
  console.error('Error: FLUXWRIGHT_SERVER_TOKEN environment variable is required.')
  process.exit(1)
}

function sendJson(res, status, payload) {
  const body = JSON.stringify(payload)
  res.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': Buffer.byteLength(body),
    'Cache-Control': 'no-store',
  })
  res.end(body)
}

async function readJsonBody(req) {
  const chunks = []
  let total = 0
  for await (const chunk of req) {
    total += chunk.length
    if (total > MAX_BODY_BYTES) {
      throw Object.assign(new Error('request body exceeds 64 KiB limit'), { status: 413 })
    }
    chunks.push(chunk)
  }
  const raw = Buffer.concat(chunks).toString('utf8')
  try {
    const parsed = JSON.parse(raw)
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      throw new Error('JSON body must be an object')
    }
    return parsed
  } catch (err) {
    throw Object.assign(new Error(`invalid JSON body: ${err.message}`), { status: 400 })
  }
}

function validateWebUrl(rawUrl) {
  if (typeof rawUrl !== 'string' || !rawUrl.trim()) {
    throw Object.assign(new Error('url is required'), { status: 400 })
  }
  let parsed
  try {
    parsed = new URL(rawUrl)
  } catch {
    throw Object.assign(new Error('url must be a valid absolute URL'), { status: 400 })
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    throw Object.assign(new Error('url must use http:// or https://'), { status: 400 })
  }
  return parsed.toString()
}

async function fluxwrightApi(path, init = {}) {
  const res = await fetch(`${SERVER_URL}${path}`, {
    ...init,
    headers: {
      Authorization: `Bearer ${SERVER_TOKEN}`,
      ...(init.body ? { 'Content-Type': 'application/json' } : {}),
      ...init.headers,
    },
  })
  const text = await res.text()
  let data = null
  try {
    data = text ? JSON.parse(text) : null
  } catch {
    data = text
  }
  if (!res.ok) {
    throw Object.assign(new Error(`Fluxwright ${path} returned ${res.status}: ${text}`), {
      status: res.status >= 400 && res.status < 500 ? res.status : 502,
    })
  }
  return data
}

async function runJob(steps, timeoutMs = 60_000) {
  const submitted = await fluxwrightApi('/v1/jobs', {
    method: 'POST',
    body: JSON.stringify({
      timeout_ms: timeoutMs,
      steps,
    }),
  })

  let job = submitted
  while (job.status === 'queued' || job.status === 'running') {
    await sleep(POLL_INTERVAL_MS)
    job = await fluxwrightApi(`/v1/jobs/${submitted.id}`)
  }

  if (job.status !== 'succeeded') {
    throw Object.assign(new Error(`render job ${job.status}: ${job.error ?? 'unknown error'}`), {
      status: 502,
    })
  }
  return job
}

function buildPdfStep(body) {
  const step = { action: 'pdf' }
  if (typeof body.format === 'string') step.format = body.format
  if (typeof body.landscape === 'boolean') step.landscape = body.landscape
  if (typeof body.print_background === 'boolean') step.print_background = body.print_background
  if (typeof body.scale === 'number') step.scale = body.scale
  if (typeof body.page_ranges === 'string') step.page_ranges = body.page_ranges
  if (typeof body.prefer_css_page_size === 'boolean') {
    step.prefer_css_page_size = body.prefer_css_page_size
  }
  if (body.margin && typeof body.margin === 'object') step.margin = body.margin
  return step
}

function buildScreenshotStep(body) {
  const step = { action: 'screenshot' }
  const kind = typeof body.type === 'string' ? body.type : 'png'
  step.type = kind
  if (typeof body.full_page === 'boolean') step.full_page = body.full_page
  if (typeof body.omit_background === 'boolean') step.omit_background = body.omit_background
  if (typeof body.quality === 'number' && (kind === 'jpeg' || kind === 'webp')) {
    step.quality = body.quality
  }
  return step
}

async function handleRender(req, res, kind) {
  const body = await readJsonBody(req)
  const url = validateWebUrl(body.url)
  const waitUntil = typeof body.wait_until === 'string' ? body.wait_until : 'networkidle'
  const timeoutMs =
    typeof body.timeout_ms === 'number' && body.timeout_ms >= 1000 && body.timeout_ms <= 300_000
      ? Math.floor(body.timeout_ms)
      : 60_000

  const renderStep = kind === 'pdf' ? buildPdfStep(body) : buildScreenshotStep(body)
  const job = await runJob(
    [{ action: 'goto', url, wait_until: waitUntil }, renderStep],
    timeoutMs,
  )

  const artifact = job.results?.[1]
  if (!artifact?.base64) {
    throw Object.assign(new Error('missing base64 artifact in job results'), { status: 502 })
  }

  const bytes = Buffer.from(artifact.base64, 'base64')
  const mimeType = artifact.mime_type ?? (kind === 'pdf' ? 'application/pdf' : 'image/png')
  const ext = mimeType.split('/')[1] ?? 'bin'

  res.writeHead(200, {
    'Content-Type': mimeType,
    'Content-Length': bytes.length,
    'Content-Disposition': `inline; filename="render.${ext}"`,
    'X-Fluxwright-Job-Id': job.id,
    'Cache-Control': 'no-store',
  })
  res.end(bytes)
}

const server = http.createServer(async (req, res) => {
  try {
    const pathname = new URL(req.url ?? '/', `http://${HOST}:${PORT}`).pathname

    if (req.method === 'GET' && pathname === '/healthz') {
      return sendJson(res, 200, { status: 'ok', upstream: SERVER_URL })
    }
    if (req.method === 'POST' && pathname === '/render/pdf') {
      return await handleRender(req, res, 'pdf')
    }
    if (req.method === 'POST' && pathname === '/render/screenshot') {
      return await handleRender(req, res, 'screenshot')
    }
    return sendJson(res, 404, { error: 'not_found' })
  } catch (err) {
    const status = typeof err.status === 'number' ? err.status : 500
    return sendJson(res, status, { error: err.message })
  }
})

server.listen(PORT, HOST, () => {
  console.log(`[pdf-service] Listening on http://${HOST}:${PORT} -> ${SERVER_URL}`)
})

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.once(signal, () => {
    console.log(`[pdf-service] Received ${signal}, shutting down...`)
    server.close(() => process.exit(0))
  })
}
