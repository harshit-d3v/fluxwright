'use strict'

const { dirname, basename, extname, join } = require('node:path')
const zlib = require('node:zlib')

const PNG_SIGNATURE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])

const CRC_TABLE = (() => {
  const table = new Uint32Array(256)
  for (let n = 0; n < 256; n++) {
    let c = n
    for (let k = 0; k < 8; k++) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1
    }
    table[n] = c >>> 0
  }
  return table
})()

function crc32(buf) {
  let c = 0xffffffff
  for (let i = 0; i < buf.length; i++) {
    c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8)
  }
  return (c ^ 0xffffffff) >>> 0
}

function paethPredictor(a, b, c) {
  const p = a + b - c
  const pa = Math.abs(p - a)
  const pb = Math.abs(p - b)
  const pc = Math.abs(p - c)
  if (pa <= pb && pa <= pc) return a
  if (pb <= pc) return b
  return c
}

function unfilterPass(raw, offset, width, height, bpp, channels, bitDepth) {
  const rowBytes = Math.ceil((width * channels * bitDepth) / 8)
  const recon = Buffer.alloc(height * rowBytes)
  for (let y = 0; y < height; y++) {
    const filter = raw[offset++]
    const rowOut = y * rowBytes
    const prevOut = (y - 1) * rowBytes
    for (let i = 0; i < rowBytes; i++) {
      const x = raw[offset++]
      const a = i >= bpp ? recon[rowOut + i - bpp] : 0
      const b = y > 0 ? recon[prevOut + i] : 0
      const c = y > 0 && i >= bpp ? recon[prevOut + i - bpp] : 0
      let val
      switch (filter) {
        case 0:
          val = x
          break
        case 1:
          val = x + a
          break
        case 2:
          val = x + b
          break
        case 3:
          val = x + ((a + b) >> 1)
          break
        case 4:
          val = x + paethPredictor(a, b, c)
          break
        default:
          throw new Error(`decodePng: unsupported PNG filter type ${filter}`)
      }
      recon[rowOut + i] = val & 0xff
    }
  }
  return { recon, nextOffset: offset, rowBytes }
}

function readSample(row, pixelIdx, sampleIdx, channels, bitDepth) {
  const idx = pixelIdx * channels + sampleIdx
  if (bitDepth === 8) return row[idx]
  if (bitDepth === 16) return row[idx * 2]
  const samplesPerByte = 8 / bitDepth
  const byteVal = row[Math.floor(idx / samplesPerByte)]
  const shift = 8 - bitDepth * ((idx % samplesPerByte) + 1)
  const mask = (1 << bitDepth) - 1
  const raw = (byteVal >> shift) & mask
  return Math.round((raw * 255) / mask)
}

function writeRgbaPixels(recon, rowBytes, width, height, colorType, bitDepth, plte, trns, out, xStart, yStart, xStep, yStep, imgWidth) {
  const channels = colorType === 0 || colorType === 3 ? 1 : colorType === 2 ? 3 : colorType === 4 ? 2 : 4
  for (let py = 0; py < height; py++) {
    const row = recon.subarray(py * rowBytes, (py + 1) * rowBytes)
    const dstY = yStart + py * yStep
    for (let px = 0; px < width; px++) {
      const dstX = xStart + px * xStep
      const dstIdx = (dstY * imgWidth + dstX) * 4
      if (colorType === 6 && bitDepth === 8) {
        const s = px * 4
        out[dstIdx] = row[s]
        out[dstIdx + 1] = row[s + 1]
        out[dstIdx + 2] = row[s + 2]
        out[dstIdx + 3] = row[s + 3]
        continue
      }
      if (colorType === 2 && bitDepth === 8) {
        const s = px * 3
        const r = row[s]
        const g = row[s + 1]
        const b = row[s + 2]
        const a =
          trns && trns.length >= 6 && r === trns[1] && g === trns[3] && b === trns[5] ? 0 : 255
        out[dstIdx] = r
        out[dstIdx + 1] = g
        out[dstIdx + 2] = b
        out[dstIdx + 3] = a
        continue
      }
      if (colorType === 3) {
        const idx = px
        let pIdx
        if (bitDepth === 8) {
          pIdx = row[idx]
        } else {
          const spb = 8 / bitDepth
          const b = row[Math.floor(idx / spb)]
          const shift = 8 - bitDepth * ((idx % spb) + 1)
          pIdx = (b >> shift) & ((1 << bitDepth) - 1)
        }
        const pOff = pIdx * 3
        out[dstIdx] = plte && pOff + 2 < plte.length ? plte[pOff] : 0
        out[dstIdx + 1] = plte && pOff + 2 < plte.length ? plte[pOff + 1] : 0
        out[dstIdx + 2] = plte && pOff + 2 < plte.length ? plte[pOff + 2] : 0
        out[dstIdx + 3] = trns && pIdx < trns.length ? trns[pIdx] : 255
        continue
      }
      if (colorType === 0) {
        const g = readSample(row, px, 0, channels, bitDepth)
        let a = 255
        if (trns && trns.length >= 2) {
          const rawMask = (1 << bitDepth) - 1
          const tVal = bitDepth === 16 ? trns.readUInt16BE(0) : trns.readUInt16BE(0) & rawMask
          const spb = Math.max(1, 8 / bitDepth)
          const rawSample =
            bitDepth === 16
              ? row.readUInt16BE(px * 2)
              : bitDepth === 8
                ? row[px]
                : (row[Math.floor(px / spb)] >> (8 - bitDepth * ((px % spb) + 1))) & rawMask
          if (rawSample === tVal) a = 0
        }
        out[dstIdx] = g
        out[dstIdx + 1] = g
        out[dstIdx + 2] = g
        out[dstIdx + 3] = a
        continue
      }
      if (colorType === 4) {
        const g = readSample(row, px, 0, channels, bitDepth)
        const a = readSample(row, px, 1, channels, bitDepth)
        out[dstIdx] = g
        out[dstIdx + 1] = g
        out[dstIdx + 2] = g
        out[dstIdx + 3] = a
        continue
      }
      if (colorType === 2) {
        const r = readSample(row, px, 0, channels, bitDepth)
        const g = readSample(row, px, 1, channels, bitDepth)
        const b = readSample(row, px, 2, channels, bitDepth)
        let a = 255
        if (trns && trns.length >= 6 && bitDepth === 16) {
          if (
            row.readUInt16BE(px * 6) === trns.readUInt16BE(0) &&
            row.readUInt16BE(px * 6 + 2) === trns.readUInt16BE(2) &&
            row.readUInt16BE(px * 6 + 4) === trns.readUInt16BE(4)
          ) {
            a = 0
          }
        }
        out[dstIdx] = r
        out[dstIdx + 1] = g
        out[dstIdx + 2] = b
        out[dstIdx + 3] = a
        continue
      }
      if (colorType === 6) {
        out[dstIdx] = readSample(row, px, 0, channels, bitDepth)
        out[dstIdx + 1] = readSample(row, px, 1, channels, bitDepth)
        out[dstIdx + 2] = readSample(row, px, 2, channels, bitDepth)
        out[dstIdx + 3] = readSample(row, px, 3, channels, bitDepth)
      }
    }
  }
}

function decodePng(buf) {
  if (!Buffer.isBuffer(buf) || buf.length < 8 || !buf.subarray(0, 8).equals(PNG_SIGNATURE)) {
    throw new Error('decodePng: invalid PNG signature')
  }
  let pos = 8
  let ihdr = null
  let plte = null
  let trns = null
  const idatChunks = []

  while (pos + 12 <= buf.length) {
    const len = buf.readUInt32BE(pos)
    const type = buf.toString('ascii', pos + 4, pos + 8)
    const dataStart = pos + 8
    const dataEnd = dataStart + len
    if (dataEnd + 4 > buf.length) {
      throw new Error(`decodePng: truncated chunk ${type}`)
    }
    const chunk = buf.subarray(dataStart, dataEnd)
    if (type === 'IHDR') {
      if (len !== 13) throw new Error('decodePng: invalid IHDR length')
      ihdr = {
        width: chunk.readUInt32BE(0),
        height: chunk.readUInt32BE(4),
        bitDepth: chunk[8],
        colorType: chunk[9],
        compression: chunk[10],
        filter: chunk[11],
        interlace: chunk[12],
      }
    } else if (type === 'PLTE') {
      plte = chunk
    } else if (type === 'tRNS') {
      trns = chunk
    } else if (type === 'IDAT') {
      idatChunks.push(chunk)
    } else if (type === 'IEND') {
      break
    }
    pos = dataEnd + 4
  }

  if (!ihdr) throw new Error('decodePng: missing IHDR chunk')
  if (ihdr.compression !== 0 || ihdr.filter !== 0) {
    throw new Error('decodePng: unsupported compression or filter method')
  }
  const channels =
    ihdr.colorType === 0 || ihdr.colorType === 3 ? 1 : ihdr.colorType === 2 ? 3 : ihdr.colorType === 4 ? 2 : 4
  const bpp = Math.max(1, Math.ceil((channels * ihdr.bitDepth) / 8))
  const raw = zlib.inflateSync(Buffer.concat(idatChunks))
  const rgba = Buffer.alloc(ihdr.width * ihdr.height * 4)

  if (ihdr.interlace === 0) {
    const { recon } = unfilterPass(raw, 0, ihdr.width, ihdr.height, bpp, channels, ihdr.bitDepth)
    writeRgbaPixels(
      recon,
      Math.ceil((ihdr.width * channels * ihdr.bitDepth) / 8),
      ihdr.width,
      ihdr.height,
      ihdr.colorType,
      ihdr.bitDepth,
      plte,
      trns,
      rgba,
      0,
      0,
      1,
      1,
      ihdr.width,
    )
  } else if (ihdr.interlace === 1) {
    const xStart = [0, 4, 0, 2, 0, 1, 0]
    const yStart = [0, 0, 4, 0, 2, 0, 1]
    const xStep = [8, 8, 4, 4, 2, 2, 1]
    const yStep = [8, 8, 8, 4, 4, 2, 2]
    let offset = 0
    for (let pass = 0; pass < 7; pass++) {
      const pw = Math.ceil((ihdr.width - xStart[pass]) / xStep[pass])
      const ph = Math.ceil((ihdr.height - yStart[pass]) / yStep[pass])
      if (pw <= 0 || ph <= 0) continue
      const res = unfilterPass(raw, offset, pw, ph, bpp, channels, ihdr.bitDepth)
      offset = res.nextOffset
      writeRgbaPixels(
        res.recon,
        res.rowBytes,
        pw,
        ph,
        ihdr.colorType,
        ihdr.bitDepth,
        plte,
        trns,
        rgba,
        xStart[pass],
        yStart[pass],
        xStep[pass],
        yStep[pass],
        ihdr.width,
      )
    }
  } else {
    throw new Error(`decodePng: unsupported interlace method ${ihdr.interlace}`)
  }

  return { width: ihdr.width, height: ihdr.height, data: rgba }
}

function makeChunk(type, data) {
  const out = Buffer.alloc(12 + data.length)
  out.writeUInt32BE(data.length, 0)
  out.write(type, 4, 4, 'ascii')
  data.copy(out, 8)
  out.writeUInt32BE(crc32(out.subarray(4, 8 + data.length)), 8 + data.length)
  return out
}

function encodePng(width, height, rgba) {
  const ihdr = Buffer.alloc(13)
  ihdr.writeUInt32BE(width >>> 0, 0)
  ihdr.writeUInt32BE(height >>> 0, 4)
  ihdr[8] = 8 // bitDepth
  ihdr[9] = 6 // colorType RGBA
  ihdr[10] = 0
  ihdr[11] = 0
  ihdr[12] = 0

  const rowStride = width * 4
  const raw = Buffer.alloc((rowStride + 1) * height)
  for (let y = 0; y < height; y++) {
    const srcOff = y * rowStride
    const dstOff = y * (rowStride + 1)
    raw[dstOff] = 0
    rgba.copy(raw, dstOff + 1, srcOff, srcOff + rowStride)
  }
  const idat = zlib.deflateSync(raw)
  return Buffer.concat([PNG_SIGNATURE, makeChunk('IHDR', ihdr), makeChunk('IDAT', idat), makeChunk('IEND', Buffer.alloc(0))])
}

function blendChannel(c, alpha, bg) {
  const a = alpha / 255
  return bg + (c - bg) * a
}

function yiqDeltaSq(d1, i1, d2, i2) {
  let r1 = d1[i1]
  let g1 = d1[i1 + 1]
  let b1 = d1[i1 + 2]
  const a1 = d1[i1 + 3]
  let r2 = d2[i2]
  let g2 = d2[i2 + 1]
  let b2 = d2[i2 + 2]
  const a2 = d2[i2 + 3]
  if (r1 === r2 && g1 === g2 && b1 === b2 && a1 === a2) return 0
  if (a1 < 255) {
    r1 = blendChannel(r1, a1, 192)
    g1 = blendChannel(g1, a1, 192)
    b1 = blendChannel(b1, a1, 192)
  }
  if (a2 < 255) {
    r2 = blendChannel(r2, a2, 192)
    g2 = blendChannel(g2, a2, 192)
    b2 = blendChannel(b2, a2, 192)
  }
  const dr = r1 - r2
  const dg = g1 - g2
  const db = b1 - b2
  if (dr === 0 && dg === 0 && db === 0) return 0
  const y = dr * 0.29889531 + dg * 0.58662247 + db * 0.11448223
  const i = dr * 0.59597799 - dg * 0.2741761 - db * 0.32180189
  const q = dr * 0.21147017 - dg * 0.52261711 + db * 0.31114694
  return 0.5053 * y * y + 0.299 * i * i + 0.1957 * q * q
}

function compareImages(expected, actual, threshold = 0.2, renderDiff = false) {
  const maxDelta = 35215 * threshold * threshold
  const outW = Math.max(expected.width, actual.width)
  const outH = Math.max(expected.height, actual.height)
  const totalPixels = outW * outH

  if (expected.width === actual.width && expected.height === actual.height && Buffer.compare(expected.data, actual.data) === 0) {
    return {
      expectedSize: [expected.width, expected.height],
      actualSize: [actual.width, actual.height],
      totalPixels,
      diffPixels: 0,
      diffRatio: 0,
      diffPng: null,
    }
  }

  let diffPixels = 0
  const diffRgba = renderDiff ? Buffer.alloc(outW * outH * 4) : null
  const eData = expected.data
  const aData = actual.data

  for (let y = 0; y < outH; y++) {
    const inExpY = y < expected.height
    const inActY = y < actual.height
    for (let x = 0; x < outW; x++) {
      const inBoth = inExpY && inActY && x < expected.width && x < actual.width
      const outIdx = (y * outW + x) * 4
      if (!inBoth) {
        diffPixels++
        if (diffRgba) {
          diffRgba[outIdx] = 255
          diffRgba[outIdx + 1] = 0
          diffRgba[outIdx + 2] = 255
          diffRgba[outIdx + 3] = 255
        }
        continue
      }
      const eIdx = (y * expected.width + x) * 4
      const aIdx = (y * actual.width + x) * 4
      if (yiqDeltaSq(eData, eIdx, aData, aIdx) > maxDelta) {
        diffPixels++
        if (diffRgba) {
          diffRgba[outIdx] = 255
          diffRgba[outIdx + 1] = 0
          diffRgba[outIdx + 2] = 255
          diffRgba[outIdx + 3] = 255
        }
      } else if (diffRgba) {
        const gray = 0.299 * aData[aIdx] + 0.587 * aData[aIdx + 1] + 0.114 * aData[aIdx + 2]
        const faded = Math.max(0, Math.min(255, Math.round(255 + (gray - 255) * 0.25)))
        diffRgba[outIdx] = faded
        diffRgba[outIdx + 1] = faded
        diffRgba[outIdx + 2] = faded
        diffRgba[outIdx + 3] = 255
      }
    }
  }

  const diffRatio = totalPixels === 0 ? 0 : diffPixels / totalPixels
  const diffPng =
    diffRgba && (diffPixels > 0 || expected.width !== actual.width || expected.height !== actual.height)
      ? encodePng(outW, outH, diffRgba)
      : null

  return {
    expectedSize: [expected.width, expected.height],
    actualSize: [actual.width, actual.height],
    totalPixels,
    diffPixels,
    diffRatio,
    diffPng,
  }
}

function isWithinTolerance(diff, opts) {
  if (diff.expectedSize[0] !== diff.actualSize[0] || diff.expectedSize[1] !== diff.actualSize[1]) {
    return false
  }
  if (opts.maxDiffPixels == null && opts.maxDiffPixelRatio == null) {
    return diff.diffPixels === 0
  }
  if (opts.maxDiffPixels != null && diff.diffPixels > opts.maxDiffPixels) return false
  if (opts.maxDiffPixelRatio != null && diff.diffRatio > opts.maxDiffPixelRatio) return false
  return true
}

function artifactPaths(nameOrPath) {
  const baselinePath = Array.isArray(nameOrPath) ? join(...nameOrPath) : nameOrPath
  const dir = dirname(baselinePath)
  const ext = extname(baselinePath)
  const stem = basename(baselinePath, ext) || 'screenshot'
  return {
    baselinePath,
    actualPath: join(dir, `${stem}-actual.png`),
    diffPath: join(dir, `${stem}-diff.png`),
  }
}

function shouldUpdateSnapshots(explicitUpdate) {
  if (explicitUpdate) return true
  return /^(1|true|yes|all)$/i.test(String(process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS ?? '').trim())
}

const DISABLE_ANIMATIONS_PAGE_FN = () => {
  if (!document.getElementById('__fw_disable_animations')) {
    const style = document.createElement('style')
    style.id = '__fw_disable_animations'
    style.textContent =
      '*, *::before, *::after { animation-duration: 0s !important; animation-delay: 0s !important; animation-iteration-count: 1 !important; animation-play-state: paused !important; transition-duration: 0s !important; transition-delay: 0s !important; scroll-behavior: auto !important; caret-color: transparent !important; }'
    ;(document.head || document.documentElement).appendChild(style)
  }
}

const RESTORE_ANIMATIONS_PAGE_FN = () => {
  const el = document.getElementById('__fw_disable_animations')
  if (el) el.remove()
}

const DISABLE_ANIMATIONS_LOCATOR_FN = (el) => {
  const doc = (el && el.ownerDocument) || document
  if (!doc.getElementById('__fw_disable_animations')) {
    const style = doc.createElement('style')
    style.id = '__fw_disable_animations'
    style.textContent =
      '*, *::before, *::after { animation-duration: 0s !important; animation-delay: 0s !important; animation-iteration-count: 1 !important; animation-play-state: paused !important; transition-duration: 0s !important; transition-delay: 0s !important; scroll-behavior: auto !important; caret-color: transparent !important; }'
    ;(doc.head || doc.documentElement).appendChild(style)
  }
}

const RESTORE_ANIMATIONS_LOCATOR_FN = (el) => {
  const doc = (el && el.ownerDocument) || document
  const s = doc.getElementById('__fw_disable_animations')
  if (s) s.remove()
}

module.exports = {
  decodePng,
  encodePng,
  compareImages,
  isWithinTolerance,
  artifactPaths,
  shouldUpdateSnapshots,
  DISABLE_ANIMATIONS_PAGE_FN,
  RESTORE_ANIMATIONS_PAGE_FN,
  DISABLE_ANIMATIONS_LOCATOR_FN,
  RESTORE_ANIMATIONS_LOCATOR_FN,
}
