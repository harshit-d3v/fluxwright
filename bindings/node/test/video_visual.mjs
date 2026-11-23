import assert from 'node:assert/strict'
import { once } from 'node:events'
import fs from 'node:fs/promises'
import http from 'node:http'
import os from 'node:os'
import { join } from 'node:path'
import { createRequire } from 'node:module'
import zlib from 'node:zlib'
import { chromium, expect } from '../addon.js'

const require = createRequire(import.meta.url)
const { encodeMjpegAvi, VideoRecorder } = require('../video.js')
const { decodePng, encodePng, compareImages, artifactPaths, shouldUpdateSnapshots } = require('../visual.js')

function assertValidVideoHeader(buf) {
  assert.ok(buf.length > 16, `video file too small: ${buf.length}`)
  const isWebm = buf[0] === 0x1a && buf[1] === 0x45 && buf[2] === 0xdf && buf[3] === 0xa3
  const isAvi = buf.subarray(0, 4).toString('ascii') === 'RIFF' && buf.subarray(8, 12).toString('ascii') === 'AVI '
  assert.ok(isWebm || isAvi, `unexpected video header: ${buf.subarray(0, 16).toString('hex')}`)
}

// 1. Pure unit checks for AVI container & PNG decoder/encoder/differ.
{
  const fakeJpeg = Buffer.from([0xff, 0xd8, 0xff, 0xd9])
  const avi = encodeMjpegAvi([fakeJpeg, fakeJpeg], 10, 320, 240)
  assert.equal(avi.subarray(0, 4).toString('ascii'), 'RIFF')
  assert.equal(avi.subarray(8, 12).toString('ascii'), 'AVI ')
  assert.equal(avi.readUInt32LE(4) + 8, avi.length)
  assert.ok(avi.includes(Buffer.from('MJPG')))
  assert.ok(avi.includes(Buffer.from('00dc')))
  assert.ok(avi.includes(Buffer.from('idx1')))

  const red = Buffer.alloc(4 * 4 * 4)
  const blue = Buffer.alloc(4 * 4 * 4)
  for (let i = 0; i < 16; i++) {
    red[i * 4] = 255
    red[i * 4 + 3] = 255
    blue[i * 4 + 2] = 255
    blue[i * 4 + 3] = 255
  }
  const redPng = encodePng(4, 4, red)
  const bluePng = encodePng(4, 4, blue)
  const decRed = decodePng(redPng)
  const decBlue = decodePng(bluePng)
  assert.equal(decRed.width, 4)
  assert.equal(decRed.height, 4)
  assert.equal(Buffer.compare(decRed.data, red), 0)

  const diff = compareImages(decRed, decBlue, 0.2, true)
  assert.equal(diff.diffPixels, 16)
  assert.equal(diff.diffRatio, 1)
  const diffDecoded = decodePng(diff.diffPng)
  assert.deepEqual([...diffDecoded.data.subarray(0, 4)], [255, 0, 255, 255])

  // Alpha-channel blind spot: opaque #C0C0C0 vs transparent (0,0,0,0) must NEVER diff as 0!
  const grayOpaque = Buffer.alloc(4 * 4)
  const transparent = Buffer.alloc(4 * 4)
  const transparentRed = Buffer.alloc(4 * 4)
  for (let i = 0; i < 4; i++) {
    grayOpaque[i * 4] = 192
    grayOpaque[i * 4 + 1] = 192
    grayOpaque[i * 4 + 2] = 192
    grayOpaque[i * 4 + 3] = 255
    transparentRed[i * 4] = 255
    transparentRed[i * 4 + 3] = 0
  }
  const alphaDiffZero = compareImages(
    { width: 2, height: 2, data: grayOpaque },
    { width: 2, height: 2, data: transparent },
    0,
    false,
  )
  assert.equal(alphaDiffZero.diffPixels, 4, 'opaque #C0C0C0 vs transparent must differ at threshold 0')
  const alphaDiffDefault = compareImages(
    { width: 2, height: 2, data: grayOpaque },
    { width: 2, height: 2, data: transparent },
    0.2,
    false,
  )
  assert.equal(alphaDiffDefault.diffPixels, 4, 'opaque #C0C0C0 vs transparent must differ at default threshold')
  const bothTransparent = compareImages(
    { width: 2, height: 2, data: transparent },
    { width: 2, height: 2, data: transparentRed },
    0,
    false,
  )
  assert.equal(bothTransparent.diffPixels, 0, 'two fully transparent pixels should match')

  // PNG validation: zero dims, invalid colorType/bitDepth, missing PLTE, truncated IDAT, grayscale tRNS 16-bit.
  const PNG_SIG = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
  const makeRawChunk = (type, data) => {
    const out = Buffer.alloc(12 + data.length)
    out.writeUInt32BE(data.length, 0)
    out.write(type, 4, 4, 'ascii')
    data.copy(out, 8)
    return out
  }
  const makeIhdr = (w, h, bitDepth, colorType) => {
    const b = Buffer.alloc(13)
    b.writeUInt32BE(w, 0)
    b.writeUInt32BE(h, 4)
    b[8] = bitDepth
    b[9] = colorType
    return makeRawChunk('IHDR', b)
  }
  const makePng = (chunks) => Buffer.concat([PNG_SIG, ...chunks, makeRawChunk('IEND', Buffer.alloc(0))])

  assert.throws(
    () => decodePng(makePng([makeIhdr(0, 4, 8, 6), makeRawChunk('IDAT', zlib.deflateSync(Buffer.alloc(0)))])),
    /invalid PNG dimensions/,
  )
  assert.throws(
    () => decodePng(makePng([makeIhdr(2, 2, 4, 2), makeRawChunk('IDAT', zlib.deflateSync(Buffer.alloc(10)))])),
    /invalid colorType/,
  )
  assert.throws(
    () => decodePng(makePng([makeIhdr(2, 2, 8, 3), makeRawChunk('IDAT', zlib.deflateSync(Buffer.alloc(6)))])),
    /PLTE/,
  )
  assert.throws(
    () => decodePng(makePng([makeIhdr(4, 4, 8, 6), makeRawChunk('IDAT', zlib.deflateSync(Buffer.alloc(5)))])),
    /truncated image data/,
  )
  // Grayscale 8-bit PNG with pixel=0 and tRNS=0x0100 (out-of-range for 8-bit) must remain opaque (a=255).
  const grayTrns = Buffer.from([0x01, 0x00])
  const grayRaw = Buffer.from([0x00, 0x00]) // 1x1 filter=0, pixel=0
  const grayPng = makePng([makeIhdr(1, 1, 8, 0), makeRawChunk('tRNS', grayTrns), makeRawChunk('IDAT', zlib.deflateSync(grayRaw))])
  const decGray = decodePng(grayPng)
  assert.deepEqual([...decGray.data], [0, 0, 0, 255])

  // artifactPaths rejects path traversal and empty filenames.
  assert.throws(() => artifactPaths('../escape.png'), TypeError)
  assert.throws(() => artifactPaths('a/../b.png'), TypeError)
  assert.throws(() => artifactPaths(['shots', '..', 'b.png']), TypeError)
  assert.throws(() => artifactPaths('./local.png'), TypeError)
  assert.throws(() => artifactPaths('dir/'), TypeError)
  assert.throws(() => artifactPaths('.'), TypeError)
  assert.throws(() => artifactPaths('..'), TypeError)
  assert.throws(() => artifactPaths('   '), TypeError)

  // shouldUpdateSnapshots respects explicit false over env var.
  const savedEnv = process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
  process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS = '1'
  try {
    assert.equal(shouldUpdateSnapshots(undefined), true)
    assert.equal(shouldUpdateSnapshots(true), true)
    assert.equal(shouldUpdateSnapshots(false), false)
  } finally {
    if (savedEnv === undefined) delete process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
    else process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS = savedEnv
  }
}

const server = http.createServer((req, res) => {
  res.setHeader('Content-Type', 'text/html')
  res.end(`<!doctype html>
    <style>
      body { margin: 0; background: #ffffff; }
      #box { position: relative; width: 80px; height: 80px; background: rgb(255, 0, 0); }
      #dot { position: absolute; top: 0; left: 0; width: 2px; height: 2px; background: rgb(255, 0, 0); }
      @keyframes pulse {
        0% { background: rgb(255, 0, 0); }
        50% { background: rgb(0, 255, 0); }
        100% { background: rgb(0, 0, 255); }
      }
      #anim { width: 40px; height: 40px; background: rgb(255, 0, 0); animation: pulse 0.2s linear infinite; }
    </style>
    <div id="box"><div id="dot"></div></div>
    <div id="anim"></div>
  `)
})
server.listen(0, '127.0.0.1')
await once(server, 'listening')
server.unref()
const url = `http://127.0.0.1:${server.address().port}/`

const tmpDir = await fs.mkdtemp(join(os.tmpdir(), 'fw-node-vv-'))
const videoDir = join(tmpDir, 'videos')
const snapDir = join(tmpDir, 'snapshots')

const browser = await chromium.launch({ maxBrowsers: 1 })
try {
  // Option validation.
  await assert.rejects(() => browser.newContext({ recordVideo: { dir: '' } }), TypeError)
  await assert.rejects(() => browser.newContext({ recordVideo: { dir: videoDir, size: { width: 0, height: 10 } } }), TypeError)

  // Page without recordVideo returns null.
  {
    const plain = await browser.newPage()
    assert.equal(plain.video(), null)
    await assert.rejects(() => expect(plain).toHaveScreenshot('', { timeout: 100 }), TypeError)
    await assert.rejects(() => expect(plain).toHaveScreenshot('x.png', { threshold: 2 }), TypeError)
    await assert.rejects(() => expect(plain).toHaveScreenshot('x.png', { animations: 'bogus' }), TypeError)
    await assert.rejects(() => expect(plain).toHaveScreenshot('x.png', { caret: 'bogus' }), TypeError)
    await assert.rejects(() => expect(plain).toHaveScreenshot('x.png', { maskColor: 'red;bad' }), TypeError)
    await assert.rejects(() => expect(plain).toHaveScreenshot('x.png', { mask: '#dot' }), TypeError)
    await assert.rejects(() => expect(plain).toHaveScreenshot('x.png', { mask: [123] }), TypeError)
    await assert.rejects(() => expect(plain).not.toHaveScreenshot('x.png'), /not is not supported/)
    await plain.close()
  }

  // Video recording via browser.newContext({ recordVideo }) + saveAs + delete.
  {
    const ctx = await browser.newContext({
      viewport: { width: 640, height: 480 },
      recordVideo: { dir: videoDir, size: { width: 640, height: 480 } },
    })
    const page = await ctx.newPage()
    const video = page.video()
    assert.ok(video !== null)

    await page.goto(url)
    await page.evaluate(() => {
      setTimeout(() => {
        document.getElementById('box').style.background = 'rgb(0, 0, 255)'
      }, 40)
    })
    await new Promise((r) => setTimeout(r, 150))
    await page.close()

    const videoPath = await video.path()
    assert.ok(videoPath.startsWith(videoDir), `unexpected path: ${videoPath}`)
    const bytes = await fs.readFile(videoPath)
    assertValidVideoHeader(bytes)

    const copyPath = join(tmpDir, 'saved', 'recording.vid')
    await video.saveAs(copyPath)
    assert.equal(Buffer.compare(await fs.readFile(copyPath), bytes), 0)

    await video.delete()
    await assert.rejects(() => fs.stat(videoPath), { code: 'ENOENT' })
    await video.delete() // idempotent
    await ctx.close()
  }

  // Video recording via browser.newPage({ recordVideo }).
  {
    const page = await browser.newPage({ recordVideo: { dir: videoDir } })
    const video = page.video()
    assert.ok(video !== null)
    await page.goto(url)
    await new Promise((r) => setTimeout(r, 60))
    await page.close()
    const videoPath = await video.path()
    assertValidVideoHeader(await fs.readFile(videoPath))
  }

  // Visual regression assertions.
  {
    const page = await browser.newPage({ viewport: { width: 320, height: 240 } })
    await page.goto(url)

    const boxShot = join(snapDir, 'box.png')
    const boxActual = join(snapDir, 'box-actual.png')
    const boxDiff = join(snapDir, 'box-diff.png')
    const envShot = join(snapDir, 'env-box.png')
    const animShot = join(snapDir, 'anim.png')
    const pageShot = join(snapDir, 'page.png')

    // Missing baseline fails cleanly.
    await assert.rejects(
      () => expect(page.locator('#box')).toHaveScreenshot(boxShot, { timeout: 250 }),
      /Baseline snapshot does not exist/,
    )
    await assert.rejects(() => fs.stat(boxShot), { code: 'ENOENT' })
    await fs.stat(boxActual)

    // Create golden via { update: true }, then exact pass.
    await expect(page.locator('#box')).toHaveScreenshot(boxShot, { update: true })
    await fs.stat(boxShot)
    await expect(page.locator('#box')).toHaveScreenshot(boxShot)

    // Create golden via FLUXWRIGHT_UPDATE_SNAPSHOTS=1.
    const prevEnv = process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
    process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS = '1'
    try {
      await expect(page.locator('#box')).toHaveScreenshot(envShot)
    } finally {
      if (prevEnv === undefined) delete process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
      else process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS = prevEnv
    }
    await fs.stat(envShot)

    // Delayed restoration exercises auto-retrying polling.
    await page.evaluate(() => {
      document.getElementById('box').style.background = 'rgb(0, 255, 0)'
      setTimeout(() => {
        document.getElementById('box').style.background = 'rgb(255, 0, 0)'
      }, 120)
    })
    await expect(page.locator('#box')).toHaveScreenshot(boxShot, { timeout: 3000 })

    // Threshold tolerance pass vs threshold: 0 fail.
    await page.evaluate(() => {
      document.getElementById('box').style.background = 'rgb(225, 0, 0)'
    })
    await expect(page.locator('#box')).toHaveScreenshot(boxShot)
    await assert.rejects(
      () => expect(page.locator('#box')).toHaveScreenshot(boxShot, { threshold: 0, timeout: 250 }),
      /Screenshot mismatch/,
    )

    // maxDiffPixels & maxDiffPixelRatio pass on 2x2 dot, fail when strict.
    await page.evaluate(() => {
      document.getElementById('box').style.background = 'rgb(255, 0, 0)'
      document.getElementById('dot').style.background = 'rgb(0, 0, 255)'
    })
    await expect(page.locator('#box')).toHaveScreenshot(boxShot, { maxDiffPixels: 16 })
    await expect(page.locator('#box')).toHaveScreenshot(boxShot, { maxDiffPixelRatio: 0.01 })

    await fs.rm(boxActual, { force: true })
    await fs.rm(boxDiff, { force: true })
    await assert.rejects(
      () => expect(page.locator('#box')).toHaveScreenshot(boxShot, { timeout: 250 }),
      (err) => {
        assert.match(err.message, /pixels differ/)
        assert.match(err.message, /ratio /)
        assert.match(err.message, /box-actual\.png/)
        assert.match(err.message, /box-diff\.png/)
        return true
      },
    )
    await fs.stat(boxActual)
    const diffImg = decodePng(await fs.readFile(boxDiff))
    let foundMagenta = false
    for (let i = 0; i < diffImg.data.length; i += 4) {
      if (
        diffImg.data[i] === 255 &&
        diffImg.data[i + 1] === 0 &&
        diffImg.data[i + 2] === 255 &&
        diffImg.data[i + 3] === 255
      ) {
        foundMagenta = true
        break
      }
    }
    assert.ok(foundMagenta, 'diff PNG must contain magenta (#ff00ff) pixels')

    // animations: 'disabled' pauses CSS animations.
    await expect(page.locator('#anim')).toHaveScreenshot(animShot, { animations: 'disabled', update: true })
    await new Promise((r) => setTimeout(r, 80))
    await expect(page.locator('#anim')).toHaveScreenshot(animShot)

    // Page-level toHaveScreenshot.
    await expect(page).toHaveScreenshot(pageShot, { update: true })
    await expect(page).toHaveScreenshot(pageShot)
    await page.evaluate(() => {
      document.body.style.background = '#000000'
    })
    await assert.rejects(
      () => expect(page).toHaveScreenshot(pageShot, { timeout: 250 }),
      /page-diff\.png/,
    )

    // mask + maskColor hides changing inner region in locator.toHaveScreenshot.
    const maskedShot = join(snapDir, 'masked.png')
    await expect(page.locator('#box')).toHaveScreenshot(maskedShot, {
      mask: [page.locator('#dot')],
      maskColor: 'rgb(0, 255, 0)',
      update: true,
    })
    await page.evaluate(() => {
      document.getElementById('dot').style.background = '#ffff00'
    })
    await expect(page.locator('#box')).toHaveScreenshot(maskedShot, {
      mask: ['#dot'],
      maskColor: 'rgb(0, 255, 0)',
      timeout: 500,
    })

    await page.close()
  }

  // Regression: self-closing page (`window.close()`) with video recording must NOT deadlock context.close() / video.path().
  {
    const ctx = await browser.newContext({ recordVideo: { dir: videoDir } })
    const opener = await ctx.newPage()
    await opener.goto(url)
    const [popup] = await Promise.all([
      opener.waitForEvent('popup'),
      opener.evaluate((u) => {
        window.open(u, '_blank')
      }, url),
    ])
    // Popup video handle must be synchronously non-null immediately upon 'popup' event!
    const popupVideo = popup.video()
    assert.ok(popupVideo !== null, 'popup.video() must be immediately non-null on waitForEvent("popup")')

    await popup.evaluate(() => window.close())
    await new Promise((r) => setTimeout(r, 100))
    // Calling pages() prunes closed popup and starts stopping its recorder cleanly.
    const remainingPages = ctx.pages()
    assert.equal(remainingPages.length, 1)

    // Self-close main page too (if permitted by browser) or close context directly.
    await ctx.close()

    const popupPath = await Promise.race([
      popupVideo.path(),
      new Promise((_, reject) => setTimeout(() => reject(new Error('popup video.path() deadlocked')), 5000)),
    ])
    assertValidVideoHeader(await fs.readFile(popupPath))
  }

  // Regression: fast-closing popup (closes immediately on load) still resolves popup.video().path().
  {
    const ctx = await browser.newContext({ recordVideo: { dir: videoDir } })
    const opener = await ctx.newPage()
    await opener.goto(url)
    let syncVideo = null
    opener.once('popup', (p) => {
      syncVideo = p.video()
    })
    const [popup] = await Promise.all([
      opener.waitForEvent('popup'),
      opener.evaluate(() => {
        const w = window.open('about:blank', '_blank')
        if (w) setTimeout(() => w.close(), 15)
      }),
    ])
    assert.ok(syncVideo !== null, 'popup.video() must be non-null inside synchronous popup listener')
    assert.equal(popup.video(), syncVideo)
    await new Promise((r) => setTimeout(r, 120))
    await ctx.close()
    const pPath = await Promise.race([
      syncVideo.path(),
      new Promise((_, reject) => setTimeout(() => reject(new Error('fast popup video.path() deadlocked')), 5000)),
    ])
    assertValidVideoHeader(await fs.readFile(pPath))
  }

  // Regression: failed startPageVideo must close the page so no browser slot leaks (maxBrowsers: 1).
  {
    const blockingFile = join(tmpDir, 'not-a-dir')
    await fs.writeFile(blockingFile, 'x')
    await assert.rejects(() => browser.newPage({ recordVideo: { dir: blockingFile } }))
    const ctx = await browser.newContext({ recordVideo: { dir: blockingFile } })
    await assert.rejects(() => ctx.newPage())
    await ctx.close()
    // Browser slot must still be free!
    const healthy = await browser.newPage()
    await healthy.goto(url)
    await healthy.close()
  }

  // Regression: unobserved VideoRecorder finish rejection must not cause UnhandledPromiseRejection.
  {
    let unhandled = null
    const onUnhandled = (err) => {
      unhandled = err
    }
    process.on('unhandledRejection', onUnhandled)
    try {
      const blockingFile = join(tmpDir, 'unwritable-dir')
      await fs.writeFile(blockingFile, 'x')
      const mockSession = {
        on() {},
        off() {},
        async send() { return {} },
        async detach() {},
      }
      const rec = new VideoRecorder(blockingFile, null, mockSession)
      await assert.rejects(() => rec.stop())
      await new Promise((r) => setTimeout(r, 30))
      assert.equal(unhandled, null, 'unobserved VideoRecorder finish error must not trigger unhandledRejection')
    } finally {
      process.off('unhandledRejection', onUnhandled)
    }
  }

  // Regression: { update: false } overrides FLUXWRIGHT_UPDATE_SNAPSHOTS=1; clip rejects timeout;
  // FS error in writeArtifact fails immediately; detached locator does not leak animation style.
  {
    const page = await browser.newPage({ viewport: { width: 320, height: 240 } })
    await page.goto(url)

    // Nested clip must reject timeout option immediately.
    await assert.rejects(
      () => expect(page).toHaveScreenshot(join(snapDir, 'clip.png'), { clip: { x: 0, y: 0, width: 10, height: 10, timeout: 1 } }),
      TypeError,
    )

    // { update: false } overrides FLUXWRIGHT_UPDATE_SNAPSHOTS=1.
    const missingWithFalse = join(snapDir, 'must-not-create.png')
    const prevEnv = process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
    process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS = '1'
    try {
      await assert.rejects(
        () => expect(page.locator('#box')).toHaveScreenshot(missingWithFalse, { update: false, timeout: 200 }),
        /Baseline snapshot does not exist/,
      )
      await assert.rejects(() => fs.stat(missingWithFalse), { code: 'ENOENT' })
    } finally {
      if (prevEnv === undefined) delete process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
      else process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS = prevEnv
    }

    // update: true with timeout: 0 still captures cleanly (requiring 2 consecutive stable frames).
    const zeroTimeoutShot = join(snapDir, 'zero-timeout.png')
    await expect(page.locator('#box')).toHaveScreenshot(zeroTimeoutShot, { update: true, timeout: 0 })
    await fs.stat(zeroTimeoutShot)

    // FS write error in update mode must fail immediately (not spin to stabilization timeout).
    const dirAsFile = join(snapDir, 'dir-as-baseline')
    await fs.mkdir(dirAsFile, { recursive: true })
    const fsStart = Date.now()
    await assert.rejects(
      () => expect(page.locator('#box')).toHaveScreenshot(dirAsFile, { update: true, timeout: 5000 }),
      (err) => !/Timed out.*stabilize/.test(err.message),
    )
    assert.ok(Date.now() - fsStart < 3000, 'FS write error should fail immediately rather than spinning to timeout')

    // Detached element during LocatorAssertions.toHaveScreenshot must not leak __fw_disable_animations style.
    await page.evaluate(() => {
      const el = document.createElement('div')
      el.id = 'ephemeral'
      el.style.cssText = 'width:20px;height:20px;background:blue;'
      document.body.appendChild(el)
      setTimeout(() => el.remove(), 40)
    })
    await assert.rejects(
      () => expect(page.locator('#ephemeral')).toHaveScreenshot(join(snapDir, 'box.png'), { timeout: 250 }),
    )
    const leakedStyle = await page.evaluate(() => Boolean(document.getElementById('__fw_disable_animations')))
    assert.equal(leakedStyle, false, '__fw_disable_animations style must be removed even if locator detaches')

    // Closed page fails fast on LocatorAssertions.toHaveScreenshot.
    const boxLoc = page.locator('#box')
    await page.close()
    const closedStart = Date.now()
    await assert.rejects(
      () => expect(boxLoc).toHaveScreenshot(join(snapDir, 'box.png'), { timeout: 5000 }),
      /page is closed/i,
    )
    assert.ok(Date.now() - closedStart < 1500, 'closed page must fail fast on locator toHaveScreenshot')
  }
} finally {
  await browser.close()
  server.close()
  await fs.rm(tmpDir, { recursive: true, force: true }).catch(() => {})
}
console.log('video_visual ok')
