import assert from 'node:assert/strict'
import { once } from 'node:events'
import http from 'node:http'

import { FluxwrightClient, FluxwrightApiError } from 'fluxwright/client'
import addon, { FluxwrightClient as AddonClient, FluxwrightApiError as AddonApiError } from '../addon.js'

assert.equal(AddonClient, FluxwrightClient, 'addon.js must re-export FluxwrightClient')
assert.equal(AddonApiError, FluxwrightApiError, 'addon.js must re-export FluxwrightApiError')
assert.equal(addon.FluxwrightClient, FluxwrightClient, 'addon default export must expose FluxwrightClient')
assert.equal(addon.FluxwrightApiError, FluxwrightApiError, 'addon default export must expose FluxwrightApiError')

// Constructor validation
assert.throws(() => new FluxwrightClient(), TypeError)
assert.throws(() => new FluxwrightClient({ baseUrl: '', token: 'tok' }), TypeError)
assert.throws(() => new FluxwrightClient({ baseUrl: 'ftp://example.com', token: 'tok' }), TypeError)
assert.throws(() => new FluxwrightClient({ baseUrl: 'http://127.0.0.1:8080', token: '' }), TypeError)
assert.throws(() => new FluxwrightClient({ baseUrl: 'http://127.0.0.1:8080', token: 'tok', timeoutMs: 0 }), TypeError)
assert.throws(() => new FluxwrightClient({ baseUrl: 'http://127.0.0.1:8080', token: 'tok', fetch: null }), TypeError)

const EXPECTED_TOKEN = 'test-secret-token-1234567890abcdef'
const calls = []
const sessionFiles = new Map()
const crawlPolls = new Map()
const jobPolls = new Map()

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = []
    req.on('data', (c) => chunks.push(c))
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')))
    req.on('error', reject)
  })
}

function sendJson(res, status, payload) {
  const data = JSON.stringify(payload)
  res.writeHead(status, { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(data) })
  res.end(data)
}

const server = http.createServer(async (req, res) => {
  const rawBody = await readBody(req)
  const jsonBody = rawBody ? JSON.parse(rawBody) : null
  const auth = req.headers.authorization || null
  calls.push({ method: req.method, url: req.url, auth, body: jsonBody })

  if (req.method === 'GET' && req.url === '/healthz') {
    return sendJson(res, 200, { status: 'ok' })
  }
  if (req.method === 'GET' && req.url === '/openapi.json') {
    return sendJson(res, 200, { openapi: '3.1.0', info: { title: 'Fluxwright HTTP API' } })
  }

  // Enforce Bearer auth on all protected routes
  if (auth !== `Bearer ${EXPECTED_TOKEN}`) {
    return sendJson(res, 401, { error: { code: 'unauthorized', message: 'a bearer token is required' } })
  }

  if (req.method === 'GET' && req.url === '/v1/stats') {
    return sendJson(res, 200, { active_pages: 1, memory_ceiling_mb: 512 })
  }
  if (req.method === 'GET' && req.url === '/metrics') {
    const text = '# TYPE fluxwright_jobs_total counter\nfluxwright_jobs_total 7\n'
    res.writeHead(200, { 'Content-Type': 'text/plain; version=0.0.4; charset=utf-8' })
    return res.end(text)
  }
  if (req.method === 'POST' && req.url === '/v1/scrape') {
    return sendJson(res, 200, {
      url: jsonBody.url,
      title: 'Scraped',
      duration_ms: 12,
      engine: 'http',
      markdown: '# Scraped',
    })
  }
  if (req.method === 'POST' && req.url === '/v1/map') {
    return sendJson(res, 200, {
      url: jsonBody.url,
      count: 2,
      links: ['https://example.com/', 'https://example.com/docs'],
      duration_ms: 8,
      engine: 'http',
    })
  }
  if (req.method === 'POST' && req.url === '/v1/crawl') {
    const id = 'crawl-1'
    crawlPolls.set(id, 0)
    return sendJson(res, 202, {
      id,
      status: 'queued',
      seed_url: jsonBody.url,
      created_at: 1000,
      started_at: null,
      finished_at: null,
      discovered: 1,
      completed: 0,
      failed: 0,
      pages: [],
    })
  }
  if (req.method === 'GET' && req.url === '/v1/crawl/crawl-1') {
    const n = (crawlPolls.get('crawl-1') ?? 0) + 1
    crawlPolls.set('crawl-1', n)
    const status = n >= 2 ? 'completed' : 'running'
    return sendJson(res, 200, {
      id: 'crawl-1',
      status,
      seed_url: 'https://example.com',
      created_at: 1000,
      started_at: 1010,
      finished_at: status === 'completed' ? 1050 : null,
      discovered: 1,
      completed: status === 'completed' ? 1 : 0,
      failed: 0,
      pages: status === 'completed' ? [{ url: 'https://example.com', depth: 0, status: 'ok', engine: 'http', title: 'Ex', duration_ms: 5 }] : [],
    })
  }
  if (req.method === 'DELETE' && req.url === '/v1/crawl/crawl-1') {
    return sendJson(res, 200, { id: 'crawl-1', status: 'cancelled' })
  }
  if (req.method === 'POST' && req.url === '/v1/jobs') {
    const id = 'job-1'
    jobPolls.set(id, 0)
    return sendJson(res, 202, {
      id,
      status: 'queued',
      submitted_at: 2000,
      started_at: null,
      finished_at: null,
      attempts: 0,
      step_index: null,
      results: null,
      error: null,
    })
  }
  if (req.method === 'GET' && req.url === '/v1/jobs/job-1') {
    const n = (jobPolls.get('job-1') ?? 0) + 1
    jobPolls.set('job-1', n)
    const status = n >= 2 ? 'succeeded' : 'running'
    return sendJson(res, 200, {
      id: 'job-1',
      status,
      submitted_at: 2000,
      started_at: 2005,
      finished_at: status === 'succeeded' ? 2020 : null,
      attempts: 1,
      step_index: 1,
      results: status === 'succeeded' ? ['ok'] : null,
      error: null,
    })
  }
  if (req.method === 'DELETE' && req.url === '/v1/jobs/job-1') {
    return sendJson(res, 200, { id: 'job-1', status: 'cancelled' })
  }
  if (req.method === 'POST' && req.url === '/v1/sessions') {
    return sendJson(res, 201, {
      id: 'sess-1',
      status: 'active',
      cdp_path: '/v1/sessions/sess-1/cdp',
      keep_alive: jsonBody?.keep_alive ?? true,
      connected: false,
      url: 'about:blank',
      profile: jsonBody?.profile ?? null,
      proxy_session: null,
      record_frames: false,
      byob: false,
      created_at: 3000,
      expires_at: 330000,
      ttl_secs: jsonBody?.ttl_secs ?? 300,
    })
  }
  if (req.method === 'GET' && req.url === '/v1/sessions') {
    return sendJson(res, 200, { sessions: [{ id: 'sess-1', status: 'active' }] })
  }
  if (req.method === 'GET' && req.url === '/v1/sessions/sess-1') {
    return sendJson(res, 200, { id: 'sess-1', status: 'active', expires_at: 330000 })
  }
  if (req.method === 'POST' && req.url === '/v1/sessions/sess-1/extend') {
    return sendJson(res, 200, { id: 'sess-1', status: 'active', expires_at: 3000 + (jsonBody?.ttl_secs ?? 300) * 1000 })
  }
  if (req.method === 'DELETE' && req.url === '/v1/sessions/sess-1') {
    return sendJson(res, 200, { id: 'sess-1', status: 'closed' })
  }
  if (req.method === 'GET' && req.url === '/v1/sessions/sess-1/files') {
    const files = [...sessionFiles.entries()].map(([name, buf]) => ({ name, size: buf.length }))
    return sendJson(res, 200, { files })
  }
  if (req.method === 'POST' && req.url === '/v1/sessions/sess-1/files') {
    const buf = Buffer.from(jsonBody.content_base64, 'base64')
    sessionFiles.set(jsonBody.name, buf)
    return sendJson(res, 201, { name: jsonBody.name, size: buf.length })
  }
  if (req.method === 'GET' && req.url.startsWith('/v1/sessions/sess-1/files/')) {
    const name = decodeURIComponent(req.url.slice('/v1/sessions/sess-1/files/'.length))
    const buf = sessionFiles.get(name)
    if (!buf) {
      return sendJson(res, 404, { error: { code: 'file_not_found', message: `file '${name}' not found` } })
    }
    res.writeHead(200, { 'Content-Type': 'application/octet-stream', 'Content-Length': buf.length })
    return res.end(buf)
  }
  if (req.method === 'DELETE' && req.url.startsWith('/v1/sessions/sess-1/files/')) {
    const name = decodeURIComponent(req.url.slice('/v1/sessions/sess-1/files/'.length))
    const existed = sessionFiles.delete(name)
    if (!existed) {
      return sendJson(res, 404, { error: { code: 'file_not_found', message: `file '${name}' not found` } })
    }
    return sendJson(res, 200, { deleted: true, name })
  }

  return sendJson(res, 404, { error: { code: 'not_found', message: 'endpoint not found' } })
})

server.listen(0, '127.0.0.1')
await once(server, 'listening')
const baseUrl = `http://127.0.0.1:${server.address().port}///`

try {
  const client = new FluxwrightClient({ baseUrl, token: EXPECTED_TOKEN, timeoutMs: 5000 })
  assert.equal(client.baseUrl, `http://127.0.0.1:${server.address().port}`, 'trailing slashes stripped')

  // Core metadata & health endpoints
  assert.deepEqual(await client.healthz(), { status: 'ok' })
  assert.equal((await client.openapi()).openapi, '3.1.0')
  assert.equal((await client.stats()).memory_ceiling_mb, 512)
  const prom = await client.metrics()
  assert.ok(prom.includes('fluxwright_jobs_total 7'))

  // Scrape & map
  const scraped = await client.scrape({ url: 'https://example.com', formats: ['markdown'] })
  assert.equal(scraped.markdown, '# Scraped')
  const mapped = await client.map({ url: 'https://example.com', include_sitemap: true })
  assert.equal(mapped.count, 2)

  // Crawl individual + polling
  const startedCrawl = await client.startCrawl({ url: 'https://example.com', max_pages: 5 })
  assert.equal(startedCrawl.status, 'queued')
  assert.equal((await client.getCrawl('crawl-1')).status, 'running')
  assert.equal((await client.cancelCrawl('crawl-1')).status, 'cancelled')

  const crawled = await client.crawl({ url: 'https://example.com' }, { pollIntervalMs: 10, timeoutMs: 2000 })
  assert.equal(crawled.status, 'completed')
  assert.equal(crawled.pages.length, 1)

  // Jobs individual + polling
  const submittedJob = await client.submitJob({ steps: [{ action: 'title' }] })
  assert.equal(submittedJob.status, 'queued')
  assert.equal((await client.getJob('job-1')).status, 'running')
  assert.equal((await client.cancelJob('job-1')).status, 'cancelled')

  const jobDone = await client.runJob({ steps: [{ action: 'title' }] }, { pollIntervalMs: 10, timeoutMs: 2000 })
  assert.equal(jobDone.status, 'succeeded')
  assert.deepEqual(jobDone.results, ['ok'])

  // Sessions
  const sess = await client.createSession({ ttl_secs: 600, profile: 'demo' })
  assert.equal(sess.id, 'sess-1')
  assert.equal((await client.listSessions()).sessions.length, 1)
  assert.equal((await client.getSession('sess-1')).status, 'active')
  assert.equal((await client.extendSession('sess-1', 900)).expires_at, 903000)

  // Session files: Buffer, Uint8Array, string upload + list + download + delete
  const binPayload = Buffer.from([0x00, 0x01, 0x02, 0xff, 0xfe])
  const up1 = await client.uploadSessionFile('sess-1', 'report 1.bin', binPayload)
  assert.equal(up1.size, binPayload.length)
  const up2 = await client.uploadSessionFile('sess-1', 'notes.txt', 'hello world')
  assert.equal(up2.size, 11)
  const up3 = await client.uploadSessionFile('sess-1', 'bytes.dat', new Uint8Array([9, 8, 7]))
  assert.equal(up3.size, 3)

  const listedFiles = await client.listSessionFiles('sess-1')
  assert.equal(listedFiles.files.length, 3)

  const dl1 = await client.downloadSessionFile('sess-1', 'report 1.bin')
  assert.ok(Buffer.isBuffer(dl1))
  assert.deepEqual(dl1, binPayload)

  const dl2 = await client.downloadSessionFile('sess-1', 'notes.txt')
  assert.equal(dl2.toString('utf8'), 'hello world')

  const delFile = await client.deleteSessionFile('sess-1', 'notes.txt')
  assert.equal(delFile.deleted, true)

  const deletedSess = await client.deleteSession('sess-1')
  assert.equal(deletedSess.status, 'closed')

  // Verify Bearer token was sent on every call
  assert.ok(calls.length >= 20)
  for (const c of calls) {
    assert.equal(c.auth, `Bearer ${EXPECTED_TOKEN}`, `missing Bearer auth on ${c.method} ${c.url}`)
  }

  // Error handling -> FluxwrightApiError with status, code, body
  const badAuthClient = new FluxwrightClient({ baseUrl, token: 'wrong-token' })
  await assert.rejects(
    () => badAuthClient.stats(),
    (err) => {
      assert.ok(err instanceof FluxwrightApiError)
      assert.equal(err.status, 401)
      assert.equal(err.code, 'unauthorized')
      assert.equal(err.message, 'a bearer token is required')
      assert.deepEqual(err.body, { error: { code: 'unauthorized', message: 'a bearer token is required' } })
      return true
    },
  )

  await assert.rejects(
    () => client.downloadSessionFile('sess-1', 'missing.bin'),
    (err) => {
      assert.ok(err instanceof FluxwrightApiError)
      assert.equal(err.status, 404)
      assert.equal(err.code, 'file_not_found')
      return true
    },
  )

  // Custom fetch injection
  const customClient = new FluxwrightClient({
    baseUrl: 'https://api.example.test',
    token: 'custom-tok',
    fetch: async (url, init) => {
      assert.equal(url, 'https://api.example.test/healthz')
      assert.equal(init.headers.Authorization, 'Bearer custom-tok')
      return new Response(JSON.stringify({ status: 'custom-ok' }), { status: 200 })
    },
  })
  assert.deepEqual(await customClient.healthz(), { status: 'custom-ok' })

  console.log('client tests passed')
} finally {
  server.close()
}
