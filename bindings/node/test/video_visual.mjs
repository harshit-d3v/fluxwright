import assert from 'node:assert/strict'
import { once } from 'node:events'
import fs from 'node:fs/promises'
import http from 'node:http'
import os from 'node:os'
import { join } from 'node:path'
import { createRequire } from 'node:module'
import { chromium, expect } from '../addon.js'

const require = createRequire(import.meta.url)
const { encodeMjpegAvi } = require('../video.js')
const { decodePng, encodePng, compareImages } = require('../visual.js')

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

    await page.close()
  }
} finally {
  await browser.close()
  server.close()
  await fs.rm(tmpDir, { recursive: true, force: true }).catch(() => {})
}
console.log('video_visual ok')
