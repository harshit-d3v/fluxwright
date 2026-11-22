'use strict'

const { createHash } = require('node:crypto')
const fs = require('node:fs/promises')
const { dirname } = require('node:path')
const { performance } = require('node:perf_hooks')

const CRC32_TABLE = (() => {
  const table = new Uint32Array(256)
  for (let i = 0; i < 256; i++) {
    let crc = i
    for (let j = 0; j < 8; j++) {
      crc = crc & 1 ? (crc >>> 1) ^ 0xedb88320 : crc >>> 1
    }
    table[i] = crc >>> 0
  }
  return table
})()

function crc32(buf) {
  let crc = 0xffffffff
  for (let i = 0; i < buf.length; i++) {
    crc = CRC32_TABLE[(crc ^ buf[i]) & 0xff] ^ (crc >>> 8)
  }
  return (crc ^ 0xffffffff) >>> 0
}

function dosDateTime(date = new Date()) {
  const year = Math.min(2107, Math.max(1980, date.getUTCFullYear())) - 1980
  const month = date.getUTCMonth() + 1
  const day = date.getUTCDate()
  const hours = date.getUTCHours()
  const minutes = date.getUTCMinutes()
  const seconds = Math.floor(date.getUTCSeconds() / 2)
  const dosTime = (hours << 11) | (minutes << 5) | seconds
  const dosDate = (year << 9) | (month << 5) | day
  return { dosTime, dosDate }
}

/**
 * Minimal zero-dependency ZIP Store writer (CRC32 + local headers + central directory + EOCD).
 */
class ZipStoreWriter {
  #entries = []

  add(name, data) {
    const buf = Buffer.isBuffer(data) ? data : Buffer.from(String(data), 'utf8')
    this.#entries.push({ name: String(name), data: buf })
  }

  toBuffer() {
    if (this.#entries.length > 0xffff) {
      throw new Error('ZipStoreWriter: too many entries for standard ZIP')
    }
    const { dosTime, dosDate } = dosDateTime()
    const chunks = []
    const records = []
    let offset = 0

    for (const entry of this.#entries) {
      const nameBuf = Buffer.from(entry.name, 'utf8')
      if (nameBuf.length > 0xffff) {
        throw new Error(`ZipStoreWriter: entry name too long: ${entry.name}`)
      }
      if (entry.data.length > 0xffffffff || offset > 0xffffffff) {
        throw new Error('ZipStoreWriter: archive exceeds 4 GiB ZIP limit')
      }
      const crc = crc32(entry.data)
      const size = entry.data.length >>> 0
      const lfh = Buffer.allocUnsafe(30 + nameBuf.length)
      lfh.writeUInt32LE(0x04034b50, 0)
      lfh.writeUInt16LE(20, 4) // version needed: 2.0
      lfh.writeUInt16LE(0x0800, 6) // UTF-8 filename flag
      lfh.writeUInt16LE(0, 8) // Store (compression = 0)
      lfh.writeUInt16LE(dosTime, 10)
      lfh.writeUInt16LE(dosDate, 12)
      lfh.writeUInt32LE(crc, 14)
      lfh.writeUInt32LE(size, 18)
      lfh.writeUInt32LE(size, 22)
      lfh.writeUInt16LE(nameBuf.length, 26)
      lfh.writeUInt16LE(0, 28)
      nameBuf.copy(lfh, 30)

      chunks.push(lfh, entry.data)
      records.push({ nameBuf, crc, size, offset: offset >>> 0 })
      offset += lfh.length + entry.data.length
    }

    if (offset > 0xffffffff) {
      throw new Error('ZipStoreWriter: archive exceeds 4 GiB ZIP limit')
    }

    const cdOffset = offset >>> 0
    let cdSize = 0
    for (const rec of records) {
      const cd = Buffer.allocUnsafe(46 + rec.nameBuf.length)
      cd.writeUInt32LE(0x02014b50, 0)
      cd.writeUInt16LE(0x0314, 4) // version made by: Unix 2.0
      cd.writeUInt16LE(20, 6)
      cd.writeUInt16LE(0x0800, 8)
      cd.writeUInt16LE(0, 10)
      cd.writeUInt16LE(dosTime, 12)
      cd.writeUInt16LE(dosDate, 14)
      cd.writeUInt32LE(rec.crc, 16)
      cd.writeUInt32LE(rec.size, 20)
      cd.writeUInt32LE(rec.size, 24)
      cd.writeUInt16LE(rec.nameBuf.length, 28)
      cd.writeUInt16LE(0, 30)
      cd.writeUInt16LE(0, 32)
      cd.writeUInt16LE(0, 34)
      cd.writeUInt16LE(0, 36)
      cd.writeUInt32LE((0o100644 << 16) >>> 0, 38)
      cd.writeUInt32LE(rec.offset, 42)
      rec.nameBuf.copy(cd, 46)
      chunks.push(cd)
      cdSize += cd.length
    }

    if (cdSize > 0xffffffff) {
      throw new Error('ZipStoreWriter: archive exceeds 4 GiB ZIP limit')
    }

    const eocd = Buffer.allocUnsafe(22)
    eocd.writeUInt32LE(0x06054b50, 0)
    eocd.writeUInt16LE(0, 4)
    eocd.writeUInt16LE(0, 6)
    eocd.writeUInt16LE(records.length, 8)
    eocd.writeUInt16LE(records.length, 10)
    eocd.writeUInt32LE(cdSize >>> 0, 12)
    eocd.writeUInt32LE(cdOffset, 16)
    eocd.writeUInt16LE(0, 20)
    chunks.push(eocd)

    return Buffer.concat(chunks)
  }
}

function validateTraceOptions(method, options, schema) {
  if (options == null) return {}
  if (typeof options !== 'object' || Array.isArray(options)) {
    throw new TypeError(`${method}: options must be an object`)
  }
  const out = {}
  for (const [key, value] of Object.entries(options)) {
    if (value === undefined) continue
    if (!Object.hasOwn(schema, key)) {
      throw new TypeError(`${method}: option '${key}' is not supported`)
    }
    const kind = schema[key]
    if (kind === 'string') {
      if (typeof value !== 'string') throw new TypeError(`${method}: ${key} must be a string`)
    } else if (kind === 'nonempty-string') {
      if (typeof value !== 'string' || !value) {
        throw new TypeError(`${method}: ${key} must be a non-empty string`)
      }
    } else if (kind === 'boolean') {
      if (typeof value !== 'boolean') throw new TypeError(`${method}: ${key} must be a boolean`)
    }
    out[key] = value
  }
  return out
}

function jpegDimensions(buf) {
  if (!Buffer.isBuffer(buf) || buf.length < 4 || buf[0] !== 0xff || buf[1] !== 0xd8) return null
  let i = 2
  while (i + 3 < buf.length) {
    if (buf[i] !== 0xff) {
      i += 1
      continue
    }
    while (i < buf.length && buf[i] === 0xff) i += 1
    if (i >= buf.length) break
    const marker = buf[i++]
    if (marker === 0xd8 || marker === 0xd9 || marker === 0x00 || (marker >= 0xd0 && marker <= 0xd7)) continue
    if (i + 1 >= buf.length) break
    const segLen = buf.readUInt16BE(i)
    if (segLen < 2 || i + segLen > buf.length) break
    if ((marker === 0xc0 || marker === 0xc1 || marker === 0xc2 || marker === 0xc3) && segLen >= 7) {
      const height = buf.readUInt16BE(i + 3)
      const width = buf.readUInt16BE(i + 5)
      if (width > 0 && height > 0) return { width, height }
    }
    i += segLen
  }
  return null
}

function headerCI(headers, name) {
  if (!headers) return undefined
  const lower = name.toLowerCase()
  for (const [k, v] of Object.entries(headers)) {
    if (k.toLowerCase() === lower) return v
  }
  return undefined
}

function buildResourceSnapshot(entry, endMonoMs) {
  const totalMs = Math.max(0, endMonoMs - entry.startMonoMs)
  const waitMs = entry.responseMonoMs != null ? Math.max(0, entry.responseMonoMs - entry.startMonoMs) : totalMs
  const receiveMs = Math.max(0, totalMs - waitMs)
  const reqHeaders = Object.entries(entry.reqHeaders || {}).map(([name, value]) => ({ name, value: String(value) }))
  const respHeaders = Object.entries(entry.respHeaders || {}).map(([name, value]) => ({ name, value: String(value) }))
  const reqObj = {
    method: entry.method,
    url: entry.url,
    httpVersion: 'HTTP/1.1',
    cookies: [],
    headers: reqHeaders,
    queryString: [],
    headersSize: -1,
    bodySize: entry.postData != null ? Buffer.byteLength(entry.postData) : 0,
  }
  if (entry.postData != null) {
    reqObj.postData = {
      mimeType: headerCI(entry.reqHeaders, 'content-type') || 'application/octet-stream',
      text: entry.postData,
    }
  }
  const response = {
    status: entry.status,
    statusText: entry.statusText,
    httpVersion: 'HTTP/1.1',
    cookies: [],
    headers: respHeaders,
    content: { size: 0, mimeType: entry.mimeType },
    redirectURL: headerCI(entry.respHeaders, 'location') || '',
    headersSize: -1,
    bodySize: -1,
  }
  if (entry.failedText != null) response._failureText = entry.failedText
  return {
    type: 'resource-snapshot',
    snapshot: {
      pageref: entry.pageId,
      startedDateTime: entry.startedDateTime,
      time: totalMs,
      _monotonicTime: entry.startMonoMs,
      request: reqObj,
      response,
      cache: {},
      timings: { send: 0, wait: waitMs, receive: receiveMs },
    },
  }
}

class Tracing {
  #contextRef
  #active = false
  #chunkActive = false
  #screenshots = false
  #snapshots = false
  #sources = false
  #name = undefined
  #title = undefined
  #originPerf = 0
  #originWall = 0
  #nextCallSeq = 0
  #nextPageSeq = 0
  #chunkSeq = 0
  #sessionGen = 0
  #suppressActions = false
  #pageIds = new WeakMap()
  #traceLines = []
  #networkLines = []
  #resources = new Map()
  #chunkFrames = 0
  #inFlightNet = new Map()
  #watchers = new Map()
  #pendingPageAttaches = new Set()
  #opChain = Promise.resolve()

  constructor(context) {
    this.#contextRef = new WeakRef(context)
  }

  #serialized(fn) {
    const next = this.#opChain.then(fn, fn)
    this.#opChain = next.catch(() => {})
    return next
  }

  #monoMs() {
    return performance.now() - this.#originPerf
  }

  #ensurePageId(page) {
    let id = this.#pageIds.get(page)
    if (!id) {
      this.#nextPageSeq += 1
      id = `page@${this.#nextPageSeq}`
      this.#pageIds.set(page, id)
    }
    return id
  }

  #pushContextOptions() {
    const entry = {
      type: 'context-options',
      version: 6,
      origin: 'library',
      browserName: 'chromium',
      platform: process.platform,
      wallTime: this.#originWall,
      monotonicTime: 0,
      sdkLanguage: 'javascript',
      options: {
        screenshots: this.#screenshots,
        snapshots: this.#snapshots,
        ...(this.#sources ? { sources: true } : {}),
      },
    }
    if (this.#title !== undefined) entry.title = this.#title
    if (this.#name !== undefined) entry.contextId = this.#name
    this.#traceLines.push(JSON.stringify(entry))
  }

  #resetChunk(name, title) {
    this.#chunkSeq += 1
    this.#name = name
    this.#title = title
    this.#traceLines = []
    this.#networkLines = []
    this.#resources = new Map()
    this.#inFlightNet.clear()
    this.#chunkFrames = 0
    this.#chunkActive = true
    this.#pushContextOptions()
    const ctx = this.#contextRef.deref()
    const nowMs = this.#monoMs()
    for (const page of ctx?.pages() ?? []) {
      const pageId = this.#ensurePageId(page)
      this.#traceLines.push(
        JSON.stringify({
          type: 'event',
          time: nowMs,
          class: 'BrowserContext',
          method: 'page',
          params: { pageId },
          pageId,
        }),
      )
    }
  }

  #flushNetwork() {
    const endMs = this.#monoMs()
    for (const [req, item] of this.#inFlightNet.entries()) {
      if (this.#chunkActive) {
        this.#networkLines.push(JSON.stringify(buildResourceSnapshot(item, endMs)))
      }
      this.#inFlightNet.delete(req)
    }
  }

  #recordJpegFrame(pageId, buf, width, height, frameSwapWallTime) {
    if (!this.#active || !this.#chunkActive || !buf?.length) return
    const dims = jpegDimensions(buf)
    const w = width && width > 0 ? width : (dims?.width ?? 1280)
    const h = height && height > 0 ? height : (dims?.height ?? 720)
    const hash = createHash('sha1').update(buf).digest('hex')
    const sha1Name = `${hash}.jpeg`
    const resKey = `resources/${sha1Name}`
    if (!this.#resources.has(resKey)) this.#resources.set(resKey, buf)
    this.#chunkFrames += 1
    this.#traceLines.push(
      JSON.stringify({
        type: 'screencast-frame',
        pageId,
        sha1: sha1Name,
        width: w,
        height: h,
        timestamp: this.#monoMs(),
        frameSwapWallTime: frameSwapWallTime ?? Date.now(),
      }),
    )
  }

  async #attachPage(page) {
    if (!this.#active || page.isClosed() || this.#watchers.has(page)) return
    const ctx = this.#contextRef.deref()
    if (!ctx) return
    const pageId = this.#ensurePageId(page)
    const selfRef = new WeakRef(this)

    const onConsole = (msg) => {
      const t = selfRef.deref()
      if (!t || !t.#active || !t.#chunkActive) return
      t.#traceLines.push(
        JSON.stringify({
          type: 'console',
          messageType: msg.type ?? 'log',
          text: msg.text ?? '',
          args: [],
          location: { url: '', lineNumber: 0, columnNumber: 0 },
          pageId,
          time: t.#monoMs(),
        }),
      )
    }

    const onPageError = (err) => {
      const t = selfRef.deref()
      if (!t || !t.#active || !t.#chunkActive) return
      t.#traceLines.push(
        JSON.stringify({
          type: 'event',
          time: t.#monoMs(),
          class: 'BrowserContext',
          method: 'pageError',
          params: {
            error: {
              error: {
                name: err?.name || 'Error',
                message: err?.message || String(err),
                stack: err?.stack || '',
              },
            },
          },
          pageId,
        }),
      )
    }

    const onRequest = (req) => {
      const t = selfRef.deref()
      if (!t || !t.#active || !t.#chunkActive) return
      t.#inFlightNet.set(req, {
        pageId,
        startedDateTime: new Date().toISOString(),
        startMonoMs: t.#monoMs(),
        method: req.method(),
        url: req.url(),
        reqHeaders: req.headers(),
        postData: req.postData(),
        responseMonoMs: null,
        status: 0,
        statusText: '',
        respHeaders: {},
        mimeType: '',
        failedText: null,
      })
    }

    const onResponse = (res) => {
      const t = selfRef.deref()
      if (!t || !t.#active || !t.#chunkActive) return
      const req = res.request()
      const nowMs = t.#monoMs()
      let item = t.#inFlightNet.get(req)
      if (!item) {
        item = {
          pageId,
          startedDateTime: new Date().toISOString(),
          startMonoMs: nowMs,
          method: req.method(),
          url: req.url(),
          reqHeaders: req.headers(),
          postData: req.postData(),
          responseMonoMs: null,
          status: 0,
          statusText: '',
          respHeaders: {},
          mimeType: '',
          failedText: null,
        }
        t.#inFlightNet.set(req, item)
      }
      const respHeaders = res.headers()
      const ct = headerCI(respHeaders, 'content-type') || ''
      item.responseMonoMs = nowMs
      item.status = res.status()
      item.statusText = res.statusText()
      item.respHeaders = respHeaders
      item.mimeType = ct.split(';')[0].trim()
    }

    const onFinished = (req) => {
      const t = selfRef.deref()
      if (!t || !t.#active || !t.#chunkActive) return
      const nowMs = t.#monoMs()
      const item = t.#inFlightNet.get(req) ?? {
        pageId,
        startedDateTime: new Date().toISOString(),
        startMonoMs: nowMs,
        method: req.method(),
        url: req.url(),
        reqHeaders: req.headers(),
        postData: req.postData(),
        responseMonoMs: null,
        status: 0,
        statusText: '',
        respHeaders: {},
        mimeType: '',
        failedText: null,
      }
      t.#inFlightNet.delete(req)
      t.#networkLines.push(JSON.stringify(buildResourceSnapshot(item, nowMs)))
    }

    const onFailed = (req) => {
      const t = selfRef.deref()
      if (!t || !t.#active || !t.#chunkActive) return
      const nowMs = t.#monoMs()
      const item = t.#inFlightNet.get(req) ?? {
        pageId,
        startedDateTime: new Date().toISOString(),
        startMonoMs: nowMs,
        method: req.method(),
        url: req.url(),
        reqHeaders: req.headers(),
        postData: req.postData(),
        responseMonoMs: null,
        status: 0,
        statusText: '',
        respHeaders: {},
        mimeType: '',
        failedText: null,
      }
      t.#inFlightNet.delete(req)
      item.failedText = req.failure()?.errorText ?? 'failed'
      t.#networkLines.push(JSON.stringify(buildResourceSnapshot(item, nowMs)))
    }

    page.on('console', onConsole)
    page.on('pageerror', onPageError)
    page.on('request', onRequest)
    page.on('response', onResponse)
    page.on('requestfinished', onFinished)
    page.on('requestfailed', onFailed)

    const watcher = {
      pageId,
      cdp: null,
      cleanup: () => {
        page.off('console', onConsole)
        page.off('pageerror', onPageError)
        page.off('request', onRequest)
        page.off('response', onResponse)
        page.off('requestfinished', onFinished)
        page.off('requestfailed', onFailed)
      },
    }
    this.#watchers.set(page, watcher)

    if (this.#screenshots && !page.isClosed()) {
      const gen = this.#sessionGen
      let cdp = null
      let onFrame = null
      try {
        cdp = await ctx.newCDPSession(page)
        if (!this.#active || this.#sessionGen !== gen || page.isClosed() || this.#watchers.get(page) !== watcher) {
          await cdp.detach().catch(() => {})
          return
        }
        onFrame = (ev) => {
          if (ev?.sessionId != null) {
            cdp.send('Page.screencastFrameAck', { sessionId: ev.sessionId }).catch(() => {})
          }
          const t = selfRef.deref()
          if (!t || !ev?.data) return
          const buf = Buffer.from(ev.data, 'base64')
          const meta = ev.metadata || {}
          t.#recordJpegFrame(
            pageId,
            buf,
            meta.deviceWidth,
            meta.deviceHeight,
            typeof meta.timestamp === 'number' ? meta.timestamp * 1000 : Date.now(),
          )
        }
        cdp.on('Page.screencastFrame', onFrame)
        await cdp.send('Page.enable').catch(() => {})
        if (!this.#active || this.#sessionGen !== gen || page.isClosed() || this.#watchers.get(page) !== watcher) {
          cdp.off('Page.screencastFrame', onFrame)
          await cdp.detach().catch(() => {})
          return
        }
        await cdp.send('Page.startScreencast', {
          format: 'jpeg',
          quality: 80,
          maxWidth: 1280,
          maxHeight: 720,
          everyNthFrame: 1,
        })
        if (!this.#active || this.#sessionGen !== gen || page.isClosed() || this.#watchers.get(page) !== watcher) {
          cdp.off('Page.screencastFrame', onFrame)
          await cdp.send('Page.stopScreencast').catch(() => {})
          await cdp.detach().catch(() => {})
          return
        }
        watcher.cdp = cdp
        watcher.cleanupCdp = () => cdp.off('Page.screencastFrame', onFrame)
      } catch {
        // Page or context closed concurrently while attaching screencast.
        if (cdp) {
          if (onFrame) cdp.off('Page.screencastFrame', onFrame)
          await cdp.detach().catch(() => {})
        }
      }
    }
  }

  async #detachPage(page) {
    const watcher = this.#watchers.get(page)
    if (!watcher) return
    this.#watchers.delete(page)
    watcher.cleanup()
    watcher.cleanupCdp?.()
    if (watcher.cdp) {
      await watcher.cdp.send('Page.stopScreencast').catch(() => {})
      await watcher.cdp.detach().catch(() => {})
    }
  }

  _isActive() {
    return this.#active
  }

  _onPageCreated(page) {
    if (!this.#active) return Promise.resolve()
    const pageId = this.#ensurePageId(page)
    if (this.#chunkActive) {
      this.#traceLines.push(
        JSON.stringify({
          type: 'event',
          time: this.#monoMs(),
          class: 'BrowserContext',
          method: 'page',
          params: { pageId },
          pageId,
        }),
      )
    }
    const p = this.#attachPage(page).catch(() => {})
    this.#pendingPageAttaches.add(p)
    p.finally(() => this.#pendingPageAttaches.delete(p))
    return p
  }

  async _onPageClosing(page) {
    if (!this.#active) return
    if (this.#chunkActive) {
      const pageId = this.#ensurePageId(page)
      this.#traceLines.push(
        JSON.stringify({
          type: 'event',
          time: this.#monoMs(),
          class: 'Page',
          method: 'close',
          params: {},
          pageId,
        }),
      )
    }
    await this.#detachPage(page)
  }

  async _dispose() {
    if (!this.#active) return
    this.#active = false
    this.#chunkActive = false
    this.#sessionGen += 1
    this.#chunkSeq += 1
    const pages = [...this.#watchers.keys()]
    await Promise.allSettled([
      ...pages.map((p) => this.#detachPage(p)),
      ...[...this.#pendingPageAttaches],
    ])
    this.#pendingPageAttaches.clear()
    this.#traceLines = []
    this.#networkLines = []
    this.#resources.clear()
    this.#inFlightNet.clear()
  }

  async _runAction(page, className, methodName, apiName, params, fn) {
    if (!this.#active || !this.#chunkActive || this.#suppressActions) return fn()
    const chunkSeq = this.#chunkSeq
    const callId = `call@${++this.#nextCallSeq}`
    const pageId = this.#ensurePageId(page)
    const startTime = this.#monoMs()
    this.#traceLines.push(
      JSON.stringify({
        type: 'before',
        callId,
        startTime,
        class: className,
        method: methodName,
        apiName,
        params: params ?? {},
        pageId,
        stepId: callId,
      }),
    )
    let errorInfo
    try {
      return await fn()
    } catch (err) {
      errorInfo =
        err instanceof Error
          ? { name: err.name || 'Error', message: err.message, stack: err.stack || '' }
          : { name: 'Error', message: String(err), stack: '' }
      throw err
    } finally {
      if (this.#active && this.#chunkActive && this.#chunkSeq === chunkSeq) {
        const after = {
          type: 'after',
          callId,
          endTime: this.#monoMs(),
        }
        if (errorInfo !== undefined) after.error = errorInfo
        this.#traceLines.push(JSON.stringify(after))
      }
    }
  }

  async #finalizeChunk(path) {
    if (this.#pendingPageAttaches.size > 0) {
      await Promise.allSettled([...this.#pendingPageAttaches])
    }
    const ctx = this.#contextRef.deref()
    const openPages = ctx ? ctx.pages().filter((p) => !p.isClosed()) : []
    if (path != null && this.#screenshots && this.#chunkFrames === 0 && openPages.length > 0) {
      const deadline = performance.now() + 250
      while (performance.now() < deadline && this.#chunkFrames === 0) {
        await new Promise((r) => setTimeout(r, 20))
      }
      if (this.#chunkFrames === 0) {
        for (const page of openPages) {
          if (page.isClosed()) continue
          const watcher = this.#watchers.get(page)
          const pageId = this.#ensurePageId(page)
          try {
            let buf = null
            if (watcher?.cdp) {
              const res = await watcher.cdp.send('Page.captureScreenshot', { format: 'jpeg', quality: 80 })
              if (res?.data) buf = Buffer.from(res.data, 'base64')
            } else if (typeof page.screenshot === 'function') {
              this.#suppressActions = true
              try {
                buf = await page.screenshot({ type: 'jpeg', quality: 80 })
              } finally {
                this.#suppressActions = false
              }
            }
            if (buf?.length) {
              this.#recordJpegFrame(pageId, buf)
            }
          } catch {}
        }
      }
    }

    await new Promise((r) => setTimeout(r, 20))
    this.#flushNetwork()

    if (path != null) {
      const zip = new ZipStoreWriter()
      const traceBody = this.#traceLines.length ? `${this.#traceLines.join('\n')}\n` : ''
      const netBody = this.#networkLines.length ? `${this.#networkLines.join('\n')}\n` : ''
      zip.add('trace.trace', traceBody)
      zip.add('trace.network', netBody)
      for (const [resKey, buf] of [...this.#resources.entries()].sort(([a], [b]) => a.localeCompare(b))) {
        zip.add(resKey, buf)
      }
      const zipBytes = zip.toBuffer()
      await fs.mkdir(dirname(path), { recursive: true })
      await fs.writeFile(path, zipBytes)
    }

    this.#chunkActive = false
    this.#traceLines = []
    this.#networkLines = []
    this.#resources.clear()
    this.#inFlightNet.clear()
  }

  async start(options) {
    const opts = validateTraceOptions('tracing.start', options, {
      name: 'string',
      title: 'string',
      screenshots: 'boolean',
      snapshots: 'boolean',
      sources: 'boolean',
    })
    return this.#serialized(async () => {
      const ctx = this.#contextRef.deref()
      if (!ctx || ctx._isClosed?.()) throw new Error('tracing.start: browser context is closed')
      if (this.#active) throw new Error('Tracing has been already started')
      this.#sessionGen += 1
      this.#active = true
      this.#screenshots = Boolean(opts.screenshots)
      this.#snapshots = Boolean(opts.snapshots)
      this.#sources = Boolean(opts.sources)
      this.#originPerf = performance.now()
      this.#originWall = Date.now()
      this.#resetChunk(opts.name, opts.title)
      for (const page of ctx.pages()) {
        await this.#attachPage(page)
      }
    })
  }

  async startChunk(options) {
    const opts = validateTraceOptions('tracing.startChunk', options, {
      name: 'string',
      title: 'string',
    })
    return this.#serialized(async () => {
      const ctx = this.#contextRef.deref()
      if (!ctx || ctx._isClosed?.()) throw new Error('tracing.startChunk: browser context is closed')
      if (!this.#active) throw new Error('Must start tracing before starting a new chunk')
      this.#resetChunk(opts.name, opts.title)
    })
  }

  async stopChunk(options) {
    const opts = validateTraceOptions('tracing.stopChunk', options, {
      path: 'nonempty-string',
    })
    return this.#serialized(async () => {
      if (!this.#active) throw new Error('Must start tracing before stopping')
      if (!this.#chunkActive) throw new Error('Must start a chunk before stopping')
      await this.#finalizeChunk(opts.path)
    })
  }

  async stop(options) {
    const opts = validateTraceOptions('tracing.stop', options, {
      path: 'nonempty-string',
    })
    return this.#serialized(async () => {
      if (!this.#active) {
        if (opts.path != null) throw new Error('Tracing has not been started')
        return
      }
      try {
        if (opts.path != null && !this.#chunkActive) {
          throw new Error('No active trace chunk; call startChunk() before stop({ path })')
        }
        if (this.#chunkActive) await this.#finalizeChunk(opts.path)
      } finally {
        await this._dispose()
      }
    })
  }
}

module.exports = { Tracing, ZipStoreWriter }
