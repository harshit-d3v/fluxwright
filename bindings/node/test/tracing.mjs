import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import fs from 'node:fs/promises'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'
import { chromium } from '../addon.js'

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

async function run() {
  const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), 'fluxwright-tracing-'))
  const server = http.createServer((req, res) => {
    if (req.url === '/api/ping') {
      res.writeHead(200, { 'Content-Type': 'application/json' })
      res.end('{"ok":true}')
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
    })
    await assert.rejects(() => context.tracing.start(), /already started/)
    await assert.rejects(() => context.tracing.stop({ path: '' }), /non-empty string/)

    await prePage.locator('#name').fill('fluxwright')
    await prePage.click('#btn')
    await prePage.evaluate(() => fetch('/api/ping').then((r) => r.json()))
    await prePage.click('#err-btn')
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

    const befores = traceEvents.filter((e) => e.type === 'before')
    const afters = traceEvents.filter((e) => e.type === 'after')
    assert.ok(
      befores.some((e) => e.apiName === 'locator.fill'),
      'missing locator.fill action',
    )
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
