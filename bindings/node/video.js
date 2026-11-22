'use strict'

const { spawn } = require('node:child_process')
const { randomUUID } = require('node:crypto')
const fs = require('node:fs/promises')
const { dirname, join } = require('node:path')

const SCREENCAP_SETTLE_MS = 25

// Valid 2x2 black baseline JPEG used only if a page closes before any frame can be captured.
const FALLBACK_JPEG = Buffer.from(
  'ffd8ffe000104a46494600010100000100010000ffdb00430006040506050406060506070706080a100a0a09090a14' +
    '0e0f0c1017141818171416161a1d251f1a1b231c1616202c20232627292a29191f2d302d283025282928ffdb004301' +
    '0707070a080a130a0a13281a161a282828282828282828282828282828282828282828282828282828282828282828' +
    '2828282828282828282828282828282828ffc0001108000200020301220002110103' +
    '1101ffc4001f0000010501010101010100000000000000000102030405060708090a0bffc400b51000020103030204' +
    '03050504040000017d01020300041105122131410613516107227114328191a1082342b1c11552d1f0243362728209' +
    '0a161718191a25262728292a3435363738393a434445464748494a535455565758595a636465666768696a73747576' +
    '7778797a838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9' +
    'cad2d3d4d5d6d7d8d9dae1e2e3e4e5e6e7e8e9eaf1f2f3f4f5f6f7f8f9faffc4001f01000301010101010101010100' +
    '00000000000102030405060708090a0bffc400b5110002010204040304070504040001027700010203110405213106' +
    '1241510761711322328108144291a1b1c109233352f0156272d10a162434e125f11718191a262728292a3536373839' +
    '3a434445464748494a535455565758595a636465666768696a737475767778797a82838485868788898a9293949596' +
    '9798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae2e3e4e5e6e7e8' +
    'e9eaf2f3f4f5f6f7f8f9faffda000c03010002110311003f00f9528a28a00fffd9',
  'hex',
)

function validateRecordVideoOptions(value, method) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError(`${method}: must be an object`)
  }
  for (const key of Object.keys(value)) {
    if (key !== 'dir' && key !== 'size') {
      throw new TypeError(`${method}: option '${key}' is not supported`)
    }
  }
  if (typeof value.dir !== 'string' || !value.dir) {
    throw new TypeError(`${method}: dir must be a non-empty string`)
  }
  if (value.size != null) {
    if (typeof value.size !== 'object' || Array.isArray(value.size)) {
      throw new TypeError(`${method}.size: must be an object`)
    }
    for (const key of Object.keys(value.size)) {
      if (key !== 'width' && key !== 'height') {
        throw new TypeError(`${method}.size: option '${key}' is not supported`)
      }
    }
    const { width, height } = value.size
    if (
      typeof width !== 'number' || !Number.isInteger(width) || width <= 0 ||
      typeof height !== 'number' || !Number.isInteger(height) || height <= 0
    ) {
      throw new TypeError(`${method}.size: width and height must be positive integers`)
    }
  }
}

function jpegDimensions(buf) {
  if (!Buffer.isBuffer(buf) || buf.length < 4 || buf[0] !== 0xff || buf[1] !== 0xd8) {
    return null
  }
  let i = 2
  while (i + 3 < buf.length) {
    if (buf[i] !== 0xff) {
      i++
      continue
    }
    while (i < buf.length && buf[i] === 0xff) i++
    if (i >= buf.length) break
    const marker = buf[i++]
    if (marker === 0x00 || marker === 0x01 || (marker >= 0xd0 && marker <= 0xd9)) {
      continue
    }
    if (i + 1 >= buf.length) break
    const segLen = buf.readUInt16BE(i)
    if (segLen < 2 || i + segLen > buf.length) break
    const isSof =
      (marker >= 0xc0 && marker <= 0xc3) ||
      (marker >= 0xc5 && marker <= 0xc7) ||
      (marker >= 0xc9 && marker <= 0xcb) ||
      (marker >= 0xcd && marker <= 0xcf)
    if (isSof && segLen >= 7) {
      const h = buf.readUInt16BE(i + 3)
      const w = buf.readUInt16BE(i + 5)
      if (w > 0 && h > 0) return { width: w, height: h }
    }
    i += segLen
  }
  return null
}

function computeFps(frames) {
  if (frames.length < 2) return 10
  const ts = frames.map((f) => f.timestamp).filter((t) => typeof t === 'number' && Number.isFinite(t))
  if (ts.length < 2) return 10
  const duration = Math.max(0.1, ts[ts.length - 1] - ts[0])
  return Math.max(1, Math.min(30, Math.round(frames.length / duration)))
}

function encodeMjpegAvi(frameBuffers, fps, width, height) {
  const safeFps = Math.max(1, fps | 0)
  const w = Math.max(1, width | 0)
  const h = Math.max(1, height | 0)
  const numFrames = frameBuffers.length
  let maxFrameSize = 0
  for (const f of frameBuffers) {
    if (f.length > maxFrameSize) maxFrameSize = f.length
  }
  const microsecPerFrame = Math.floor(1_000_000 / safeFps)

  const avih = Buffer.alloc(56)
  avih.writeUInt32LE(microsecPerFrame, 0)
  avih.writeUInt32LE((maxFrameSize * safeFps) >>> 0, 4)
  avih.writeUInt32LE(0, 8)
  avih.writeUInt32LE(0x10, 12) // AVIF_HASINDEX
  avih.writeUInt32LE(numFrames >>> 0, 16)
  avih.writeUInt32LE(0, 20)
  avih.writeUInt32LE(1, 24)
  avih.writeUInt32LE(maxFrameSize >>> 0, 28)
  avih.writeUInt32LE(w >>> 0, 32)
  avih.writeUInt32LE(h >>> 0, 36)

  const strh = Buffer.alloc(56)
  strh.write('vids', 0, 4, 'ascii')
  strh.write('MJPG', 4, 4, 'ascii')
  strh.writeUInt32LE(0, 8)
  strh.writeUInt16LE(0, 12)
  strh.writeUInt16LE(0, 14)
  strh.writeUInt32LE(0, 16)
  strh.writeUInt32LE(1, 20)
  strh.writeUInt32LE(safeFps >>> 0, 24)
  strh.writeUInt32LE(0, 28)
  strh.writeUInt32LE(numFrames >>> 0, 32)
  strh.writeUInt32LE(maxFrameSize >>> 0, 36)
  strh.writeUInt32LE(0xffffffff, 40)
  strh.writeUInt32LE(0, 44)
  strh.writeInt16LE(0, 48)
  strh.writeInt16LE(0, 50)
  strh.writeInt16LE(Math.min(0x7fff, w), 52)
  strh.writeInt16LE(Math.min(0x7fff, h), 54)

  const strf = Buffer.alloc(40)
  strf.writeUInt32LE(40, 0)
  strf.writeInt32LE(w, 4)
  strf.writeInt32LE(h, 8)
  strf.writeUInt16LE(1, 12)
  strf.writeUInt16LE(24, 14)
  strf.write('MJPG', 16, 4, 'ascii')
  strf.writeUInt32LE((w * h * 3) >>> 0, 20)

  const strlPayloadLen = 4 + (8 + strh.length) + (8 + strf.length)
  const hdrlPayloadLen = 4 + (8 + avih.length) + (8 + strlPayloadLen)

  let moviPayloadLen = 4
  const offsets = []
  for (const f of frameBuffers) {
    offsets.push({ offset: moviPayloadLen, size: f.length })
    moviPayloadLen += 8 + f.length + (f.length & 1)
  }
  const idx1Len = numFrames * 16
  const riffPayloadLen = 4 + (8 + hdrlPayloadLen) + (8 + moviPayloadLen) + (8 + idx1Len)

  const out = Buffer.alloc(8 + riffPayloadLen)
  let pos = 0
  const writeStr = (s) => {
    out.write(s, pos, s.length, 'ascii')
    pos += s.length
  }
  const writeU32 = (v) => {
    out.writeUInt32LE(v >>> 0, pos)
    pos += 4
  }

  writeStr('RIFF')
  writeU32(riffPayloadLen)
  writeStr('AVI ')

  writeStr('LIST')
  writeU32(hdrlPayloadLen)
  writeStr('hdrl')

  writeStr('avih')
  writeU32(avih.length)
  avih.copy(out, pos)
  pos += avih.length

  writeStr('LIST')
  writeU32(strlPayloadLen)
  writeStr('strl')

  writeStr('strh')
  writeU32(strh.length)
  strh.copy(out, pos)
  pos += strh.length

  writeStr('strf')
  writeU32(strf.length)
  strf.copy(out, pos)
  pos += strf.length

  writeStr('LIST')
  writeU32(moviPayloadLen)
  writeStr('movi')
  for (const f of frameBuffers) {
    writeStr('00dc')
    writeU32(f.length)
    f.copy(out, pos)
    pos += f.length
    if (f.length & 1) out[pos++] = 0
  }

  writeStr('idx1')
  writeU32(idx1Len)
  for (const entry of offsets) {
    writeStr('00dc')
    writeU32(0x10) // AVIIF_KEYFRAME
    writeU32(entry.offset)
    writeU32(entry.size)
  }

  return out
}

async function tryMuxWebm(outPath, frames, fps, size, firstDims) {
  const args = [
    '-y',
    '-loglevel',
    'error',
    '-f',
    'image2pipe',
    '-vcodec',
    'mjpeg',
    '-framerate',
    String(fps),
    '-i',
    '-',
  ]
  const hasMixedDims =
    firstDims != null &&
    frames.some((f) => {
      const d = jpegDimensions(f.jpeg)
      return d && (d.width !== firstDims.width || d.height !== firstDims.height)
    })
  const scaleTarget = size ?? (hasMixedDims ? firstDims : null)
  if (scaleTarget) {
    const { width: w, height: h } = scaleTarget
    args.push('-vf', `scale=${w}:${h}:force_original_aspect_ratio=decrease,pad=${w}:${h}:(ow-iw)/2:(oh-ih)/2`)
  }
  args.push('-c:v', 'libvpx', '-b:v', '1M', '-pix_fmt', 'yuv420p', '-an', outPath)

  const ok = await new Promise((resolve) => {
    let settled = false
    const finish = (val) => {
      if (!settled) {
        settled = true
        resolve(val)
      }
    }
    let child
    try {
      child = spawn('ffmpeg', args, { stdio: ['pipe', 'ignore', 'ignore'], shell: false })
    } catch {
      return finish(false)
    }
    child.on('error', () => finish(false))
    child.stdin.on('error', () => {})
    child.on('close', (code) => finish(code === 0))
    try {
      for (const f of frames) {
        child.stdin.write(f.jpeg)
      }
      child.stdin.end()
    } catch {
      finish(false)
    }
  })

  if (!ok) return false
  try {
    const data = await fs.readFile(outPath)
    return data.length >= 4 && data[0] === 0x1a && data[1] === 0x45 && data[2] === 0xdf && data[3] === 0xa3
  } catch {
    return false
  }
}

class Video {
  #recorder

  constructor(recorder) {
    this.#recorder = recorder
  }

  async path() {
    return this.#recorder.waitForFinished()
  }

  async saveAs(path) {
    if (typeof path !== 'string' || !path) {
      throw new TypeError('video.saveAs: path must be a non-empty string')
    }
    const src = await this.path()
    await fs.mkdir(dirname(path), { recursive: true })
    await fs.copyFile(src, path)
  }

  async delete() {
    const src = await this.path()
    await fs.rm(src, { force: true })
  }
}

class VideoRecorder {
  #dir
  #size
  #id = randomUUID()
  #session
  #frames = []
  #onFrame
  #finishedPromise
  #resolveFinished
  #rejectFinished
  #stopping = null

  constructor(dir, size, session) {
    this.#dir = dir
    this.#size = size ?? null
    this.#session = session
    this.#finishedPromise = new Promise((resolve, reject) => {
      this.#resolveFinished = resolve
      this.#rejectFinished = reject
    })
    this.#onFrame = (params) => {
      if (params?.data) {
        try {
          const jpeg = Buffer.from(params.data, 'base64')
          const meta = params.metadata || {}
          this.#frames.push({
            jpeg,
            timestamp: typeof meta.timestamp === 'number' ? meta.timestamp : null,
            width: typeof meta.deviceWidth === 'number' ? Math.max(1, Math.round(meta.deviceWidth)) : null,
            height: typeof meta.deviceHeight === 'number' ? Math.max(1, Math.round(meta.deviceHeight)) : null,
          })
        } catch {}
      }
      if (params?.sessionId !== undefined) {
        this.#session.send('Page.screencastFrameAck', { sessionId: params.sessionId }).catch(() => {})
      }
    }
  }

  static async start(session, options) {
    await fs.mkdir(options.dir, { recursive: true })
    const recorder = new VideoRecorder(options.dir, options.size ?? null, session)
    session.on('Page.screencastFrame', recorder.#onFrame)
    await session.send('Page.enable').catch(() => {})
    const startParams = {
      format: 'jpeg',
      quality: 80,
      everyNthFrame: 1,
    }
    if (options.size) {
      startParams.maxWidth = options.size.width
      startParams.maxHeight = options.size.height
    }
    await session.send('Page.startScreencast', startParams)
    return recorder
  }

  waitForFinished() {
    return this.#finishedPromise
  }

  stop() {
    if (!this.#stopping) {
      this.#stopping = this.#finish()
    }
    return this.#stopping
  }

  async #finish() {
    try {
      await this.#session.send('Page.stopScreencast').catch(() => {})
      await new Promise((r) => setTimeout(r, SCREENCAP_SETTLE_MS))
      this.#session.off('Page.screencastFrame', this.#onFrame)

      if (this.#frames.length === 0) {
        const shot = await this.#session
          .send('Page.captureScreenshot', { format: 'jpeg', quality: 80 })
          .catch(() => null)
        if (shot?.data) {
          this.#frames.push({
            jpeg: Buffer.from(shot.data, 'base64'),
            timestamp: null,
            width: this.#size?.width ?? null,
            height: this.#size?.height ?? null,
          })
        }
      }
      await this.#session.detach().catch(() => {})

      const frames = this.#frames.length
        ? this.#frames
        : [{ jpeg: FALLBACK_JPEG, timestamp: null, width: 2, height: 2 }]

      await fs.mkdir(this.#dir, { recursive: true })
      const fps = computeFps(frames)
      const f0 = frames[0]
      const firstDims =
        jpegDimensions(f0.jpeg) ?? (f0.width && f0.height ? { width: f0.width, height: f0.height } : null)
      const dims = this.#size ?? firstDims ?? { width: 800, height: 600 }

      const webmPath = join(this.#dir, `${this.#id}.webm`)
      if (await tryMuxWebm(webmPath, frames, fps, this.#size, firstDims)) {
        this.#resolveFinished(webmPath)
        return webmPath
      }
      await fs.rm(webmPath, { force: true }).catch(() => {})

      const aviPath = join(this.#dir, `${this.#id}.avi`)
      const aviBuf = encodeMjpegAvi(
        frames.map((f) => f.jpeg),
        fps,
        dims.width,
        dims.height,
      )
      await fs.writeFile(aviPath, aviBuf)
      this.#resolveFinished(aviPath)
      return aviPath
    } catch (err) {
      this.#rejectFinished(err)
      throw err
    }
  }
}

module.exports = {
  Video,
  VideoRecorder,
  validateRecordVideoOptions,
  encodeMjpegAvi,
  jpegDimensions,
}
