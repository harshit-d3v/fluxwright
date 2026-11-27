'use strict'

const CRAWL_TERMINAL_STATUSES = new Set(['completed', 'failed', 'cancelled'])
const JOB_TERMINAL_STATUSES = new Set(['succeeded', 'failed', 'cancelled'])

class FluxwrightApiError extends Error {
  constructor(message, { status, code = null, body = null } = {}) {
    super(message)
    this.name = 'FluxwrightApiError'
    this.status = status
    this.code = code
    this.body = body
  }
}

function validateNonEmptyString(value, name) {
  if (typeof value !== 'string' || !value.trim()) {
    throw new TypeError(`FluxwrightClient: ${name} must be a non-empty string`)
  }
  return value.trim()
}

function validatePositiveNumber(value, name) {
  if (typeof value !== 'number' || !Number.isFinite(value) || value <= 0) {
    throw new TypeError(`FluxwrightClient: ${name} must be a positive finite number`)
  }
  return value
}

function normalizeBaseUrl(baseUrl) {
  const raw = validateNonEmptyString(baseUrl, 'baseUrl')
  let parsed
  try {
    parsed = new URL(raw)
  } catch {
    throw new TypeError(`FluxwrightClient: baseUrl must be a valid http(s) URL: ${raw}`)
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    throw new TypeError(`FluxwrightClient: baseUrl must use http:// or https://: ${raw}`)
  }
  return raw.replace(/\/+$/, '')
}

function encodeSegment(value, name) {
  return encodeURIComponent(validateNonEmptyString(value, name))
}

function toBase64(content) {
  if (typeof content === 'string') {
    return Buffer.from(content, 'utf8').toString('base64')
  }
  if (Buffer.isBuffer(content)) {
    return content.toString('base64')
  }
  if (content instanceof Uint8Array) {
    return Buffer.from(content.buffer, content.byteOffset, content.byteLength).toString('base64')
  }
  throw new TypeError('FluxwrightClient.uploadSessionFile: content must be a Buffer, Uint8Array, or string')
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

class FluxwrightClient {
  #baseUrl
  #token
  #timeoutMs
  #fetch

  constructor({ baseUrl, token, timeoutMs = 30000, fetch: fetchImpl = globalThis.fetch } = {}) {
    this.#baseUrl = normalizeBaseUrl(baseUrl)
    this.#token = validateNonEmptyString(token, 'token')
    this.#timeoutMs = validatePositiveNumber(timeoutMs, 'timeoutMs')
    if (typeof fetchImpl !== 'function') {
      throw new TypeError('FluxwrightClient: fetch implementation is required (Node >= 18 or custom fetch option)')
    }
    this.#fetch = fetchImpl
  }

  get baseUrl() {
    return this.#baseUrl
  }

  async #request(method, path, { body, responseType = 'json', timeoutMs } = {}) {
    const effectiveTimeout = timeoutMs === undefined ? this.#timeoutMs : validatePositiveNumber(timeoutMs, 'timeoutMs')
    const url = `${this.#baseUrl}${path}`
    const headers = {
      Authorization: `Bearer ${this.#token}`,
    }
    if (responseType === 'json') {
      headers.Accept = 'application/json'
    } else if (responseType === 'text') {
      headers.Accept = 'text/plain'
    }

    const init = { method, headers }
    if (body !== undefined) {
      headers['Content-Type'] = 'application/json'
      init.body = JSON.stringify(body)
    }

    const controller = new AbortController()
    init.signal = controller.signal
    let timedOut = false
    const timer = setTimeout(() => {
      timedOut = true
      controller.abort()
    }, effectiveTimeout)
    if (typeof timer.unref === 'function') timer.unref()

    let res
    try {
      res = await this.#fetch(url, init)
    } catch (err) {
      if (timedOut) {
        throw new Error(`FluxwrightClient: ${method} ${path} timed out after ${effectiveTimeout}ms`, { cause: err })
      }
      throw err
    } finally {
      clearTimeout(timer)
    }

    if (!res.ok) {
      const rawText = await res.text()
      let parsed = null
      if (rawText) {
        try {
          parsed = JSON.parse(rawText)
        } catch {
          parsed = null
        }
      }
      const errObj = parsed && typeof parsed === 'object' ? (parsed.error ?? parsed) : null
      const code = errObj && typeof errObj.code === 'string' ? errObj.code : null
      const message =
        (errObj && typeof errObj.message === 'string' && errObj.message) ||
        (rawText && rawText.trim()) ||
        `HTTP ${res.status}${res.statusText ? ` ${res.statusText}` : ''}`
      throw new FluxwrightApiError(message, {
        status: res.status,
        code,
        body: parsed ?? (rawText || null),
      })
    }

    if (responseType === 'buffer') {
      return Buffer.from(await res.arrayBuffer())
    }
    if (responseType === 'text') {
      return res.text()
    }
    const text = await res.text()
    if (!text) return {}
    return JSON.parse(text)
  }

  healthz() {
    return this.#request('GET', '/healthz')
  }

  openapi() {
    return this.#request('GET', '/openapi.json')
  }

  stats() {
    return this.#request('GET', '/v1/stats')
  }

  metrics() {
    return this.#request('GET', '/metrics', { responseType: 'text' })
  }

  scrape(request) {
    if (!request || typeof request !== 'object') {
      throw new TypeError('FluxwrightClient.scrape: request must be an object')
    }
    return this.#request('POST', '/v1/scrape', { body: request })
  }

  map(request) {
    if (!request || typeof request !== 'object') {
      throw new TypeError('FluxwrightClient.map: request must be an object')
    }
    return this.#request('POST', '/v1/map', { body: request })
  }

  startCrawl(request) {
    if (!request || typeof request !== 'object') {
      throw new TypeError('FluxwrightClient.startCrawl: request must be an object')
    }
    return this.#request('POST', '/v1/crawl', { body: request })
  }

  getCrawl(id) {
    return this.#request('GET', `/v1/crawl/${encodeSegment(id, 'id')}`)
  }

  cancelCrawl(id) {
    return this.#request('DELETE', `/v1/crawl/${encodeSegment(id, 'id')}`)
  }

  async crawl(request, { pollIntervalMs = 250, timeoutMs } = {}) {
    const interval = validatePositiveNumber(pollIntervalMs, 'pollIntervalMs')
    const totalTimeout = timeoutMs === undefined ? this.#timeoutMs : validatePositiveNumber(timeoutMs, 'timeoutMs')
    const deadline = Date.now() + totalTimeout

    let snapshot = await this.startCrawl(request)
    while (!CRAWL_TERMINAL_STATUSES.has(snapshot?.status)) {
      const remaining = deadline - Date.now()
      if (remaining <= 0) {
        throw new Error(`FluxwrightClient.crawl: crawl ${snapshot?.id ?? ''} timed out after ${totalTimeout}ms`)
      }
      await sleep(Math.min(interval, remaining))
      const reqRemaining = deadline - Date.now()
      if (reqRemaining <= 0) {
        throw new Error(`FluxwrightClient.crawl: crawl ${snapshot?.id ?? ''} timed out after ${totalTimeout}ms`)
      }
      snapshot = await this.#request('GET', `/v1/crawl/${encodeSegment(snapshot.id, 'id')}`, {
        timeoutMs: Math.min(this.#timeoutMs, reqRemaining),
      })
    }
    return snapshot
  }

  submitJob(request) {
    if (!request || typeof request !== 'object') {
      throw new TypeError('FluxwrightClient.submitJob: request must be an object')
    }
    return this.#request('POST', '/v1/jobs', { body: request })
  }

  getJob(id) {
    return this.#request('GET', `/v1/jobs/${encodeSegment(id, 'id')}`)
  }

  cancelJob(id) {
    return this.#request('DELETE', `/v1/jobs/${encodeSegment(id, 'id')}`)
  }

  async runJob(request, { pollIntervalMs = 200, timeoutMs } = {}) {
    const interval = validatePositiveNumber(pollIntervalMs, 'pollIntervalMs')
    const totalTimeout = timeoutMs === undefined ? this.#timeoutMs : validatePositiveNumber(timeoutMs, 'timeoutMs')
    const deadline = Date.now() + totalTimeout

    let job = await this.submitJob(request)
    while (!JOB_TERMINAL_STATUSES.has(job?.status)) {
      const remaining = deadline - Date.now()
      if (remaining <= 0) {
        throw new Error(`FluxwrightClient.runJob: job ${job?.id ?? ''} timed out after ${totalTimeout}ms`)
      }
      await sleep(Math.min(interval, remaining))
      const reqRemaining = deadline - Date.now()
      if (reqRemaining <= 0) {
        throw new Error(`FluxwrightClient.runJob: job ${job?.id ?? ''} timed out after ${totalTimeout}ms`)
      }
      job = await this.#request('GET', `/v1/jobs/${encodeSegment(job.id, 'id')}`, {
        timeoutMs: Math.min(this.#timeoutMs, reqRemaining),
      })
    }
    return job
  }

  createSession(request = {}) {
    if (!request || typeof request !== 'object') {
      throw new TypeError('FluxwrightClient.createSession: request must be an object')
    }
    return this.#request('POST', '/v1/sessions', { body: request })
  }

  listSessions() {
    return this.#request('GET', '/v1/sessions')
  }

  getSession(id) {
    return this.#request('GET', `/v1/sessions/${encodeSegment(id, 'id')}`)
  }

  extendSession(id, ttlSecs) {
    const body = ttlSecs === undefined ? {} : { ttl_secs: validatePositiveNumber(ttlSecs, 'ttlSecs') }
    return this.#request('POST', `/v1/sessions/${encodeSegment(id, 'id')}/extend`, { body })
  }

  deleteSession(id) {
    return this.#request('DELETE', `/v1/sessions/${encodeSegment(id, 'id')}`)
  }

  listSessionFiles(id) {
    return this.#request('GET', `/v1/sessions/${encodeSegment(id, 'id')}/files`)
  }

  uploadSessionFile(id, name, content) {
    const fileName = validateNonEmptyString(name, 'name')
    const contentBase64 = toBase64(content)
    return this.#request('POST', `/v1/sessions/${encodeSegment(id, 'id')}/files`, {
      body: { name: fileName, content_base64: contentBase64 },
    })
  }

  downloadSessionFile(id, name) {
    return this.#request('GET', `/v1/sessions/${encodeSegment(id, 'id')}/files/${encodeSegment(name, 'name')}`, {
      responseType: 'buffer',
    })
  }

  deleteSessionFile(id, name) {
    return this.#request('DELETE', `/v1/sessions/${encodeSegment(id, 'id')}/files/${encodeSegment(name, 'name')}`)
  }
}

module.exports = {
  FluxwrightClient,
  FluxwrightApiError,
}
