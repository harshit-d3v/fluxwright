import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import fs from 'node:fs/promises'
import http from 'node:http'
import { createRequire } from 'node:module'
import os from 'node:os'
import path from 'node:path'
import { chromium } from '../addon.js'

const require = createRequire(import.meta.url)
const { Tracing } = require('../tracing.js')

const CRC32_TABLE = (() => {
  const table = new Uint32Array(256)
  for (let i = 0; i < 256; i++) {
    let c = i
    for (let k = 0; k < 8; k++) c = (c & 1) !== 0 ? 0xedb88320 ^ (c >>> 1) : c >>> 1
    table[i] = c >>> 0
  }
  return table
})()

function crc32(buf) {
  let c = 0xffffffff
  for (let i = 0; i < buf.length; i++) c = CRC32_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8)
  return (c ^ 0xffffffff) >>> 0
}

function parseZipStore(buf) {
  assert.ok(buf.length >= 22, 'zip too small')
  assert.equal(buf.readUInt32LE(0), 0x04034b50, 'missing PK\\x03\\x04 magic')
  let eocdOffset = -1
  for (let i = buf.length - 22; i >= Math.max(0, buf.length - 65557); i--) {
    if (buf.readUInt32LE(i) === 0x06054b50) {
      eocdOffset = i
      break
    }
  }
  assert.ok(eocdOffset >= 0, 'missing ZIP EOCD')
  const totalEntries = buf.readUInt16LE(eocdOffset + 10)
  const cdSize = buf.readUInt32LE(eocdOffset + 12)
  const cdOffset = buf.readUInt32LE(eocdOffset + 16)
  assert.equal(cdOffset + cdSize, eocdOffset, 'unexpected central directory bounds')

  const entries = new Map()
  let cursor = cdOffset
  for (let idx = 0; idx < totalEntries; idx++) {
    assert.equal(buf.readUInt32LE(cursor), 0x02014b50, 'bad central directory signature')
    const method = buf.readUInt16LE(cursor + 10)
    assert.equal(method, 0, 'expected Store compression method (0)')
    const expectedCrc = buf.readUInt32LE(cursor + 16)
    const compSize = buf.readUInt32LE(cursor + 20)
    const uncompSize = buf.readUInt32LE(cursor + 24)
    assert.equal(compSize, uncompSize)
    const nameLen = buf.readUInt16LE(cursor + 28)
    const extraLen = buf.readUInt16LE(cursor + 30)
    const commentLen = buf.readUInt16LE(cursor + 32)
    const localOffset = buf.readUInt32LE(cursor + 42)
    const name = buf.subarray(cursor + 46, cursor + 46 + nameLen).toString('utf8')
    cursor += 46 + nameLen + extraLen + commentLen

    assert.equal(buf.readUInt32LE(localOffset), 0x04034b50, `bad local header for ${name}`)
    const lNameLen = buf.readUInt16LE(localOffset + 26)
    const lExtraLen = buf.readUInt16LE(localOffset + 28)
    const dataStart = localOffset + 30 + lNameLen + lExtraLen
    const data = buf.subarray(dataStart, dataStart + uncompSize)
    assert.equal(crc32(data), expectedCrc, `CRC32 mismatch for ${name}`)
    entries.set(name, data)
  }
  return entries
}

function parseNdjson(buf, label) {
  const text = buf.toString('utf8').trim()
  if (!text) return []
  return text.split('\n').map((line, i) => {
    try {
      return JSON.parse(line)
    } catch (err) {
      throw new assert.AssertionError({ message: `${label} line ${i + 1} invalid JSON: ${err.message}` })
    }
  })
}

function findSnapshotElements(node, tag, out = []) {
  if (!Array.isArray(node)) return out
  if (node[0] === tag) out.push(node)
  for (let i = 2; i < node.length; i++) {
    findSnapshotElements(node[i], tag, out)
  }
  return out
}

function countSnapshotNodesAndText(node) {
  if (typeof node === 'string') return { nodes: 1, chars: node.length }
  if (!Array.isArray(node)) return { nodes: 0, chars: 0 }
  let nodes = 1
  let chars = 0
  for (let i = 2; i < node.length; i++) {
    const child = countSnapshotNodesAndText(node[i])
    nodes += child.nodes
    chars += child.chars
  }
  return { nodes, chars }
}

async function run() {
  const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'fluxwright-tracing-'))
  const server = http.createServer((req, res) => {
    if (req.url === '/api/ping') {
      res.writeHead(200, { 'Content-Type': 'application/json' })
      res.end('{"ok":true}')
      return
    }
    if (req.url === '/form') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' })
      res.end(`<!doctype html>
        <html>
          <head><title>Form Snapshots</title></head>
          <body>
            <input id="name" type="text" />
            <input id="agree" type="checkbox" />
            <textarea id="bio"></textarea>
            <select id="role">
              <option value="dev">Dev</option>
              <option value="ops">Ops</option>
            </select>
            <script>window.__node_secret__ = "SECRET_NODE_SCRIPT_123";</script>
          </body>
        </html>`)
      return
    }
    if (req.url === '/large') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' })
      const spans = Array.from({ length: 4500 }, (_, i) => `<span>n${i}</span>`).join('')
      const bigText = 'Y'.repeat(25000)
      res.end(`<!doctype html><html><body><div id="wrap">${spans}</div><pre id="big">${bigText}</pre></body></html>`)
      return
    }
    res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' })
    res.end(`<!doctype html>
      <html>
        <body style="margin:0;padding:16px;background:#fafafa">
          <h1 id="heading">Trace Demo</h1>
          <input id="name" type="text" />
          <button id="btn" onclick="document.getElementById('out').textContent='clicked'; console.log('btn-clicked')">Go</button>
          <button id="err-btn" onclick="setTimeout(() => { throw new Error('trace-boom') }, 0)">Boom</button>
          <div id="out">idle</div>
          <script>console.log('page-loaded')</script>
        </body>
      </html>`)
  })
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
  const { port } = server.address()
  const baseUrl = `http://127.0.0.1:${port}`

  const browser = await chromium.launch({ headless: true })
  try {
    const context = await browser.newContext()

    // Error checks before start()
    await assert.rejects(() => context.tracing.stop({ path: path.join(tmpDir, 'never.zip') }), /not been started/)
    await assert.rejects(() => context.tracing.startChunk({ title: 'bad' }), /Must start tracing/)
    await assert.rejects(() => context.tracing.stopChunk({ path: path.join(tmpDir, 'bad.zip') }), /Must start tracing/)
    // No-op stop without path before start()
    await context.tracing.stop()

    const prePage = await context.newPage()
    await prePage.goto(baseUrl)

    const tracePath = path.join(tmpDir, 'nested', 'trace.zip')
    await context.tracing.start({
      name: 'e2e-ctx',
      title: 'Initial Trace',
      screenshots: true,
      snapshots: true,
      sources: true,
    })
    await assert.rejects(() => context.tracing.start(), /already started/)
    await assert.rejects(() => context.tracing.stop({ path: '' }), /non-empty string/)

    await prePage.locator('#name').fill('fluxwright')
    await prePage.click('#btn')
    await prePage.evaluate(() => fetch('/api/ping').then((r) => r.json()))
    await prePage.click('#err-btn')
    await assert.rejects(() => prePage.locator('#nonexistent-for-trace').hover({ timeout: 120 }))
    await new Promise((r) => setTimeout(r, 80))

    const subPage = await context.newPage()
    await subPage.goto(`${baseUrl}/second`)
    await subPage.close()

    await context.tracing.stop({ path: tracePath })

    const entries = parseZipStore(await fs.readFile(tracePath))
    assert.ok(entries.has('trace.trace'), 'missing trace.trace')
    assert.ok(entries.has('trace.network'), 'missing trace.network')

    const traceEvents = parseNdjson(entries.get('trace.trace'), 'trace.trace')
    const ctxOpts = traceEvents.find((e) => e.type === 'context-options')
    assert.ok(ctxOpts, 'missing context-options entry')
    assert.equal(ctxOpts.title, 'Initial Trace')
    assert.equal(ctxOpts.contextId, 'e2e-ctx')
    assert.equal(ctxOpts.options?.screenshots, true)
    assert.equal(ctxOpts.options?.snapshots, true)
    assert.equal(ctxOpts.options?.sources, true)

    const befores = traceEvents.filter((e) => e.type === 'before')
    const afters = traceEvents.filter((e) => e.type === 'after')
    const locatorFillBefore = befores.find((e) => e.apiName === 'locator.fill')
    assert.ok(locatorFillBefore, 'missing locator.fill action')
    assert.equal(locatorFillBefore.params?.selector, '#name', 'locator traced action must populate params.selector')

    const failedHoverBefore = befores.find(
      (e) => e.apiName === 'locator.hover' && e.params?.selector === '#nonexistent-for-trace',
    )
    assert.ok(failedHoverBefore, 'missing failed locator.hover action')
    const failedHoverAfter = afters.find((e) => e.callId === failedHoverBefore.callId)
    assert.ok(failedHoverAfter?.error, 'failed traced action must record after.error')
    assert.equal(typeof failedHoverAfter.error.name, 'string')
    assert.ok(failedHoverAfter.error.message.length > 0)
    assert.equal(typeof failedHoverAfter.error.stack, 'string')
    assert.ok(
      befores.some((e) => e.apiName === 'page.click'),
      'missing page.click action',
    )
    assert.ok(
      !befores.some((e) => e.apiName === 'locator.click'),
      'page.click should not double-record locator.click',
    )
    assert.ok(
      befores.some((e) => e.apiName === 'page.goto'),
      'missing page.goto action',
    )
    assert.ok(afters.length >= befores.length, 'missing after records')

    const consoles = traceEvents.filter((e) => e.type === 'console')
    assert.ok(
      consoles.some((e) => e.text.includes('btn-clicked')),
      'missing console event',
    )

    const pageErr = traceEvents.find((e) => e.type === 'event' && e.method === 'pageError')
    assert.ok(pageErr, 'missing pageError event')
    assert.match(pageErr.params.error.error.message, /trace-boom/)

    const frames = traceEvents.filter((e) => e.type === 'screencast-frame')
    assert.ok(frames.length >= 1, 'expected at least 1 screencast-frame')
    for (const f of frames) {
      assert.match(f.sha1, /^[0-9a-f]{40}\.jpeg$/)
      const key = `resources/${f.sha1}`
      const jpeg = entries.get(key)
      assert.ok(jpeg, `missing resource ${key}`)
      assert.equal(jpeg[0], 0xff)
      assert.equal(jpeg[1], 0xd8)
      assert.equal(jpeg[2], 0xff)
      assert.equal(`${createHash('sha1').update(jpeg).digest('hex')}.jpeg`, f.sha1)
    }

    const netEvents = parseNdjson(entries.get('trace.network'), 'trace.network')
    assert.ok(
      netEvents.some((e) => e.type === 'resource-snapshot' && e.snapshot.request.url.includes('/api/ping') && e.snapshot.response.status === 200),
      'missing /api/ping network snapshot',
    )

    // Verify frame-snapshot events when snapshots: true
    const snapshotsByName = new Map(
      traceEvents.filter((e) => e.type === 'frame-snapshot').map((e) => [e.snapshot.snapshotName, e.snapshot]),
    )
    assert.ok(snapshotsByName.size > 0, 'expected frame-snapshot events when snapshots: true')
    const locatorFillAfter = afters.find((e) => e.callId === locatorFillBefore.callId)
    assert.equal(locatorFillBefore.beforeSnapshot, `before@${locatorFillBefore.callId}`)
    assert.equal(locatorFillAfter?.afterSnapshot, `after@${locatorFillBefore.callId}`)
    const fillAfterSnap = snapshotsByName.get(locatorFillAfter.afterSnapshot)
    assert.ok(fillAfterSnap, 'missing frame-snapshot for locator.fill afterSnapshot')
    assert.equal(fillAfterSnap.callId, locatorFillBefore.callId)
    assert.equal(fillAfterSnap.frameId, 'main')
    assert.equal(fillAfterSnap.doctype, 'html')
    assert.equal(fillAfterSnap.isMainFrame, true)
    assert.deepEqual(fillAfterSnap.resourceOverrides, [])
    assert.ok(fillAfterSnap.frameUrl.startsWith(baseUrl))
    assert.ok(fillAfterSnap.viewport?.width > 0 && fillAfterSnap.viewport?.height > 0)
    assert.equal(fillAfterSnap.html[0], 'HTML')
    const filledInputs = findSnapshotElements(fillAfterSnap.html, 'INPUT')
    assert.ok(
      filledInputs.some((el) => el[1]?.id === 'name' && el[1]?.__playwright_value__ === 'fluxwright'),
      'expected live filled input value as __playwright_value__ in afterSnapshot',
    )
    const strippedScripts = findSnapshotElements(fillAfterSnap.html, 'SCRIPT')
    assert.ok(strippedScripts.length >= 1)
    for (const sc of strippedScripts) assert.deepEqual(sc, ['SCRIPT', {}])

    // Multi-chunk workflow test
    const chunk1Path = path.join(tmpDir, 'chunk1.zip')
    const chunk2Path = path.join(tmpDir, 'chunk2.zip')
    await context.tracing.start({ title: 'Chunk Zero', screenshots: true })
    await prePage.click('#btn')
    await context.tracing.stopChunk({ path: chunk1Path })
    await assert.rejects(() => context.tracing.stopChunk({ path: chunk2Path }), /Must start a chunk/)
    await context.tracing.startChunk({ name: 'part-2', title: 'Chunk Two' })
    await prePage.goto(`${baseUrl}/chunk2`)
    await context.tracing.stopChunk({ path: chunk2Path })
    await context.tracing.stop()

    const c1 = parseNdjson(parseZipStore(await fs.readFile(chunk1Path)).get('trace.trace'), 'chunk1')
    const c2 = parseNdjson(parseZipStore(await fs.readFile(chunk2Path)).get('trace.trace'), 'chunk2')
    assert.equal(c1.find((e) => e.type === 'context-options')?.title, 'Chunk Zero')
    assert.equal(c2.find((e) => e.type === 'context-options')?.title, 'Chunk Two')
    assert.ok(c2.some((e) => e.type === 'before' && e.apiName === 'page.goto'))
    assert.ok(!c2.some((e) => e.type === 'before' && e.apiName === 'page.click'))
    // snapshots: false emits 0 frame-snapshot events
    assert.equal(c1.filter((e) => e.type === 'frame-snapshot').length, 0)
    assert.equal(c2.filter((e) => e.type === 'frame-snapshot').length, 0)
    assert.ok(c1.filter((e) => e.type === 'before').every((e) => e.beforeSnapshot === undefined))
    assert.ok(c1.filter((e) => e.type === 'after').every((e) => e.afterSnapshot === undefined))

    // Regression: cross-chunk action isolation (action spanning stopChunk -> startChunk must not emit orphan after).
    const cross1 = path.join(tmpDir, 'cross1.zip')
    const cross2 = path.join(tmpDir, 'cross2.zip')
    await context.tracing.start({ title: 'Cross 1', screenshots: false })
    const slowEval = prePage.evaluate(() => new Promise((r) => setTimeout(() => r('done'), 220)))
    await new Promise((r) => setTimeout(r, 30))
    await context.tracing.stopChunk({ path: cross1 })
    await context.tracing.startChunk({ title: 'Cross 2' })
    assert.equal(await slowEval, 'done')
    await context.tracing.stopChunk({ path: cross2 })
    // Regression: stop({ path }) after stopChunk() throws AND tears down session cleanly so restart succeeds.
    await assert.rejects(() => context.tracing.stop({ path: path.join(tmpDir, 'bad-stop.zip') }), /No active trace chunk/)
    assert.equal(context.tracing._isActive(), false, 'stop({ path }) after stopChunk() must dispose tracing session')

    const cross2Events = parseNdjson(parseZipStore(await fs.readFile(cross2)).get('trace.trace'), 'cross2')
    const cross2BeforeIds = new Set(cross2Events.filter((e) => e.type === 'before').map((e) => e.callId))
    for (const ev of cross2Events.filter((e) => e.type === 'after')) {
      assert.ok(cross2BeforeIds.has(ev.callId), `orphaned after event ${ev.callId} leaked into next chunk`)
    }

    // Regression: failed ZIP write preserves chunk so retrying stopChunk with a valid path succeeds.
    const dirTarget = path.join(tmpDir, 'dir-as-zip')
    await fs.mkdir(dirTarget, { recursive: true })
    const retryZip = path.join(tmpDir, 'retry-ok.zip')
    await context.tracing.start({ title: 'Retry Chunk', screenshots: false })
    await prePage.click('#btn')
    await assert.rejects(() => context.tracing.stopChunk({ path: dirTarget }))
    await context.tracing.stopChunk({ path: retryZip })
    await context.tracing.stop()
    const retryEvents = parseNdjson(parseZipStore(await fs.readFile(retryZip)).get('trace.trace'), 'retry')
    assert.ok(retryEvents.some((e) => e.type === 'before' && e.apiName === 'page.click'))

    // Regression: multi-page fallback screenshot captures ALL open pages via page.screenshot() when watcher.cdp is null.
    {
      const tinyJpeg = Buffer.from([
        0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
        0x00, 0x01, 0x00, 0x00, 0xff, 0xd9,
      ])
      const makeMockPage = (tag) => ({
        isClosed: () => false,
        on() {},
        off() {},
        async screenshot(opts) {
          assert.equal(opts?.type, 'jpeg')
          return Buffer.concat([tinyJpeg.subarray(0, tinyJpeg.length - 2), Buffer.from(tag), Buffer.from([0xff, 0xd9])])
        },
      })
      const p1 = makeMockPage('P1')
      const p2 = makeMockPage('P2')
      const mockCtx = {
        _isClosed: () => false,
        pages: () => [p1, p2],
        async newCDPSession() {
          throw new Error('no cdp')
        },
      }
      const tr = new Tracing(mockCtx)
      const multiZip = path.join(tmpDir, 'multi-fallback.zip')
      await tr.start({ screenshots: true })
      await tr.stop({ path: multiZip })
      const multiEntries = parseZipStore(await fs.readFile(multiZip))
      const multiEvents = parseNdjson(multiEntries.get('trace.trace'), 'multi-fallback')
      const multiFrames = multiEvents.filter((e) => e.type === 'screencast-frame')
      assert.equal(multiFrames.length, 2, 'multi-page fallback must capture screenshots for all open pages')
      assert.notEqual(multiFrames[0].pageId, multiFrames[1].pageId)
    }

    // Regression: standalone DOM snapshots (snapshots: true, screenshots: false), form state, bounds, and cross-chunk isolation.
    {
      const standaloneZip = path.join(tmpDir, 'dom-standalone.zip')
      await context.tracing.start({ screenshots: false, snapshots: true })
      await prePage.goto(`${baseUrl}/form`)
      await prePage.locator('#name').fill('Alice')
      await prePage.locator('#agree').check()
      await prePage.locator('#bio').fill('NodeTrace')
      await prePage.locator('#role').selectOption('ops')
      await context.tracing.stop({ path: standaloneZip })

      const standaloneEntries = parseZipStore(await fs.readFile(standaloneZip))
      const traceText = standaloneEntries.get('trace.trace').toString('utf8')
      assert.ok(!traceText.includes('SECRET_NODE_SCRIPT_123'), 'script body must be stripped')
      assert.ok(![...standaloneEntries.keys()].some((k) => k.endsWith('.jpeg')), 'screenshots: false must emit 0 JPEGs')

      const stEvents = parseNdjson(standaloneEntries.get('trace.trace'), 'dom-standalone')
      assert.equal(stEvents.filter((e) => e.type === 'screencast-frame').length, 0)
      const snapMap = new Map(
        stEvents.filter((e) => e.type === 'frame-snapshot').map((e) => [e.snapshot.snapshotName, e.snapshot]),
      )
      assert.ok(snapMap.size > 0)
      for (const b of stEvents.filter((e) => e.type === 'before')) {
        assert.ok(snapMap.has(b.beforeSnapshot), `missing beforeSnapshot ${b.beforeSnapshot}`)
      }
      for (const a of stEvents.filter((e) => e.type === 'after')) {
        assert.ok(snapMap.has(a.afterSnapshot), `missing afterSnapshot ${a.afterSnapshot}`)
      }
      const lastAfter = stEvents.filter((e) => e.type === 'after').at(-1)
      const finalHtml = snapMap.get(lastAfter.afterSnapshot).html
      const inputs = findSnapshotElements(finalHtml, 'INPUT')
      assert.ok(inputs.some((el) => el[1]?.id === 'name' && el[1]?.__playwright_value__ === 'Alice'))
      assert.ok(inputs.some((el) => el[1]?.id === 'agree' && el[1]?.__playwright_checked__ === true))
      assert.ok(
        findSnapshotElements(finalHtml, 'TEXTAREA').some((el) => el[1]?.id === 'bio' && el[1]?.__playwright_value__ === 'NodeTrace'),
      )
      assert.ok(
        findSnapshotElements(finalHtml, 'SELECT').some((el) => el[1]?.id === 'role' && el[1]?.__playwright_value__ === 'ops'),
      )
      const options = findSnapshotElements(finalHtml, 'OPTION')
      assert.ok(options.some((el) => el[1]?.value === 'dev' && el[1]?.__playwright_selected__ === false))
      assert.ok(options.some((el) => el[1]?.value === 'ops' && el[1]?.__playwright_selected__ === true))
      for (const sc of findSnapshotElements(finalHtml, 'SCRIPT')) assert.deepEqual(sc, ['SCRIPT', {}])

      // Bounded DOM caps (4000 nodes, 16384 text chars)
      const boundsZip = path.join(tmpDir, 'dom-bounds.zip')
      await context.tracing.start({ screenshots: false, snapshots: true })
      await prePage.goto(`${baseUrl}/large`)
      await prePage.evaluate(() => 1 + 1)
      await context.tracing.stop({ path: boundsZip })
      const boundsEvents = parseNdjson(parseZipStore(await fs.readFile(boundsZip)).get('trace.trace'), 'dom-bounds')
      const evalBefore = boundsEvents.find((e) => e.type === 'before' && e.apiName === 'page.evaluate')
      const largeSnap = boundsEvents.find((e) => e.type === 'frame-snapshot' && e.snapshot.snapshotName === evalBefore.beforeSnapshot)
      const { nodes, chars } = countSnapshotNodesAndText(largeSnap.snapshot.html)
      assert.ok(nodes <= 4000, `node count ${nodes} exceeded 4000`)
      assert.ok(chars <= 16384, `text length ${chars} exceeded 16384`)

      // Cross-chunk snapshot isolation with snapshots: true
      const snapIso1 = path.join(tmpDir, 'snap-iso1.zip')
      const snapIso2 = path.join(tmpDir, 'snap-iso2.zip')
      await context.tracing.start({ screenshots: false, snapshots: true })
      const slowSnapEval = prePage.evaluate(() => new Promise((r) => setTimeout(() => r('ok'), 220)))
      await new Promise((r) => setTimeout(r, 40))
      await context.tracing.stopChunk({ path: snapIso1 })
      await context.tracing.startChunk({ title: 'Snap Iso 2' })
      assert.equal(await slowSnapEval, 'ok')
      await context.tracing.stopChunk({ path: snapIso2 })
      await context.tracing.stop()

      const s1Events = parseNdjson(parseZipStore(await fs.readFile(snapIso1)).get('trace.trace'), 'snap-iso1')
      const s2Events = parseNdjson(parseZipStore(await fs.readFile(snapIso2)).get('trace.trace'), 'snap-iso2')
      const s1Before = s1Events.find((e) => e.type === 'before')
      assert.ok(s1Before)
      assert.ok(!s2Events.some((e) => e.type === 'after' && e.callId === s1Before.callId))
      assert.ok(!s2Events.some((e) => e.type === 'frame-snapshot' && e.snapshot.callId === s1Before.callId))
    }

    // Unit test: lightweight CDP session for snapshots: true, screenshots: false (no screencast calls) and timeout/error swallowing.
    {
      const cdpCalls = []
      let sessionCount = 0
      const mockPage = { isClosed: () => false, on() {}, off() {} }
      const makeMockCtx = (evalImpl) => ({
        _isClosed: () => false,
        pages: () => [mockPage],
        async newCDPSession() {
          sessionCount += 1
          return {
            on() {},
            off() {},
            async send(method, params) {
              cdpCalls.push({ method, params })
              if (method === 'Runtime.evaluate') return evalImpl()
              return {}
            },
            async detach() {
              cdpCalls.push({ method: 'detach' })
            },
          }
        },
      })

      // snapshots: false, screenshots: false opens 0 CDP sessions
      const trZero = new Tracing(makeMockCtx(async () => ({})))
      const zeroZip = path.join(tmpDir, 'unit-zero.zip')
      await trZero.start({ screenshots: false, snapshots: false })
      await trZero._runAction(mockPage, 'Page', 'click', 'page.click', { selector: '#a' }, async () => 'ok')
      await trZero.stop({ path: zeroZip })
      assert.equal(sessionCount, 0, 'snapshots: false, screenshots: false must open 0 CDP sessions')

      // snapshots: true, screenshots: false opens CDP session without Page.enable/startScreencast/stopScreencast, and swallows errors/timeouts
      const trSnapOnly = new Tracing(makeMockCtx(async () => new Promise(() => {}))) // stalls past 500ms timeout
      const snapTimeoutZip = path.join(tmpDir, 'unit-snap-timeout.zip')
      await trSnapOnly.start({ screenshots: false, snapshots: true })
      const res = await trSnapOnly._runAction(mockPage, 'Page', 'click', 'page.click', { selector: '#a' }, async () => 'done')
      assert.equal(res, 'done')
      await trSnapOnly.stop({ path: snapTimeoutZip })
      assert.equal(sessionCount, 1)
      assert.ok(!cdpCalls.some((c) => c.method === 'Page.enable' || c.method === 'Page.startScreencast' || c.method === 'Page.stopScreencast'))
      assert.ok(cdpCalls.some((c) => c.method === 'Runtime.evaluate' && c.params?.returnByValue === true))
      assert.ok(cdpCalls.some((c) => c.method === 'detach'))
      const tEvents = parseNdjson(parseZipStore(await fs.readFile(snapTimeoutZip)).get('trace.trace'), 'unit-snap-timeout')
      assert.equal(tEvents.filter((e) => e.type === 'frame-snapshot').length, 0)
      const tBefore = tEvents.find((e) => e.type === 'before')
      const tAfter = tEvents.find((e) => e.type === 'after')
      assert.equal(tBefore.beforeSnapshot, undefined)
      assert.equal(tAfter.afterSnapshot, undefined)
    }

    await context.close()
    console.log('tracing.mjs: ALL PASS')
  } finally {
    await browser.close()
    server.close()
    await fs.rm(tmpDir, { recursive: true, force: true })
  }
}

run().catch((err) => {
  console.error(err)
  process.exit(1)
})
