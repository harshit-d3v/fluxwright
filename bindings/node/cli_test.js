#!/usr/bin/env node
'use strict'

const { fork, spawnSync } = require('node:child_process')
const fs = require('node:fs/promises')
const path = require('node:path')
const { writeHtmlReport, formatDuration } = require('./html_report.js')
const { _internals } = require('./test_runner.js')

const TEST_FILE_RE = /\.(?:test|spec)\.(?:js|mjs|cjs|ts|mts|cts)$/i
const CONFIG_FILENAMES = [
  'fluxwright.config.mjs',
  'fluxwright.config.js',
  'fluxwright.config.cjs',
  'fluxwright.config.ts',
]
const IGNORE_DIRS = new Set([
  'node_modules',
  '.git',
  'target',
  'fluxwright-report',
  'test-results',
  '.venv',
  'venv',
  '__pycache__',
  'dist',
  'build',
])

function printHelp() {
  process.stdout.write(`Usage: fluxwright-test [options] [paths...]

Options:
  -c, --config=<path>          Path to fluxwright.config.{mjs,js,cjs,ts}
  --workers=<N>, -j <N>        Number of parallel worker processes (default: 1)
  --shard=<k/n>                Run shard k of n (1-indexed, e.g. 1/2)
  --retries=<N>                Max retries per failed test (default: 0)
  --grep=<pattern>, -g <pat>   Only run tests matching regex/substring
  --grep-invert=<pattern>      Skip tests matching regex/substring
  --update-snapshots, -u       Set FLUXWRIGHT_UPDATE_SNAPSHOTS=1
  --reporter=<list>            Comma-separated: list,html,json (default: list)
  --output=<dir>               Artifacts directory (default: test-results)
  --report-dir=<dir>           HTML report directory (default: fluxwright-report)
  --headed                     Run browsers in headed mode
  --screenshot=<mode>          off | on | only-on-failure (default: only-on-failure)
  --trace=<mode>               off | on | retain-on-failure (default: off)
  --video=<mode>               off | on | retain-on-failure (default: off)
  --timeout=<ms>               Per-test timeout in ms (default: 30000)
  --max-browsers=<N>           Max pooled browsers per worker
  -h, --help                   Show this help message
  -v, --version                Show version
`)
}

const MAX_NFA_STATES = 10_000
const MAX_QUANTIFIER_REPEAT = 1000

function isWordChar(ch) {
  if (!ch) return false
  const c = ch.charCodeAt(0)
  return (c >= 48 && c <= 57) || (c >= 65 && c <= 90) || (c >= 97 && c <= 122) || c === 95
}

function isDigitChar(ch) {
  const c = ch.charCodeAt(0)
  return c >= 48 && c <= 57
}

function isSpaceChar(ch) {
  return ch === ' ' || ch === '\t' || ch === '\n' || ch === '\r' || ch === '\f' || ch === '\v' || ch === '\u00a0' || ch === '\ufeff'
}

function isLineBreak(ch) {
  return ch === '\n' || ch === '\r' || ch === '\u2028' || ch === '\u2029'
}

function parseHexEscape(source, pos, digits) {
  if (pos + digits > source.length) return null
  const slice = source.slice(pos, pos + digits)
  for (let i = 0; i < slice.length; i++) {
    const c = slice.charCodeAt(i)
    const isHex = (c >= 48 && c <= 57) || (c >= 65 && c <= 70) || (c >= 97 && c <= 102)
    if (!isHex) return null
  }
  return { char: String.fromCharCode(parseInt(slice, 16)), nextPos: pos + digits }
}

function charPredForEscape(esc, ignoreCase) {
  if (esc === 'd') return isDigitChar
  if (esc === 'D') return (ch) => !isDigitChar(ch)
  if (esc === 'w') return isWordChar
  if (esc === 'W') return (ch) => !isWordChar(ch)
  if (esc === 's') return isSpaceChar
  if (esc === 'S') return (ch) => !isSpaceChar(ch)
  const map = { n: '\n', r: '\r', t: '\t', f: '\f', v: '\v', '0': '\0' }
  const lit = Object.hasOwn(map, esc) ? map[esc] : esc
  if (ignoreCase) {
    const lower = lit.toLowerCase()
    return (ch) => ch.toLowerCase() === lower
  }
  return (ch) => ch === lit
}

/**
 * Linear-time Thompson NFA regex compiler (RE2-style, ReDoS-proof).
 * Supports real regex syntax (/pattern/flags, character classes, shorthand escapes,
 * grouping, alternation, quantifiers, and anchors) without calling `new RegExp`
 * on untrusted CLI arguments.
 */
function compileGrepMatcher(pattern, flag = '--grep') {
  const raw = String(pattern)
  let source = raw
  let flags = ''
  if (raw.length >= 2 && raw.startsWith('/')) {
    const lastSlash = raw.lastIndexOf('/')
    if (lastSlash > 0) {
      const candidateFlags = raw.slice(lastSlash + 1)
      if (/^[dgimsuvy]*$/.test(candidateFlags)) {
        const seen = new Set()
        for (const f of candidateFlags) {
          if (seen.has(f)) {
            throw new Error(`Invalid ${flag} regex "${raw}": duplicate flag "${f}"`)
          }
          seen.add(f)
        }
        if (seen.has('u') && seen.has('v')) {
          throw new Error(`Invalid ${flag} regex "${raw}": incompatible flags "u" and "v"`)
        }
        source = raw.slice(1, lastSlash)
        flags = candidateFlags.replace(/g/g, '')
      }
    }
  }

  const ignoreCase = flags.includes('i')
  const multiline = flags.includes('m')
  const dotAll = flags.includes('s')
  const sticky = flags.includes('y')

  const fail = (msg) => {
    throw new Error(`Invalid ${flag} regex "${raw}": ${msg}`)
  }

  let pos = 0

  function readEscapeChar() {
    if (pos >= source.length) fail('Trailing backslash')
    const ch = source[pos++]
    if (ch === 'x') {
      const hex = parseHexEscape(source, pos, 2)
      if (hex) {
        pos = hex.nextPos
        return { kind: 'char', value: hex.char }
      }
      return { kind: 'char', value: 'x' }
    }
    if (ch === 'u') {
      if (source[pos] === '{') {
        const end = source.indexOf('}', pos + 1)
        if (end !== -1) {
          const hexStr = source.slice(pos + 1, end)
          if (/^[0-9a-fA-F]+$/.test(hexStr)) {
            const cp = parseInt(hexStr, 16)
            if (cp > 0x10ffff) fail('Unicode code point out of range')
            pos = end + 1
            return { kind: 'char', value: String.fromCodePoint(cp) }
          }
        }
      }
      const hex = parseHexEscape(source, pos, 4)
      if (hex) {
        pos = hex.nextPos
        return { kind: 'char', value: hex.char }
      }
      return { kind: 'char', value: 'u' }
    }
    if (ch === 'd' || ch === 'D' || ch === 'w' || ch === 'W' || ch === 's' || ch === 'S') {
      return { kind: 'class', pred: charPredForEscape(ch, ignoreCase) }
    }
    const map = { n: '\n', r: '\r', t: '\t', f: '\f', v: '\v', '0': '\0' }
    return { kind: 'char', value: Object.hasOwn(map, ch) ? map[ch] : ch }
  }

  function parseCharClass() {
    pos += 1 // consume '['
    let negated = false
    if (pos < source.length && source[pos] === '^') {
      negated = true
      pos += 1
    }
    const preds = []
    while (pos < source.length) {
      if (source[pos] === ']') {
        pos += 1
        const combined = (ch) => {
          const matched = preds.some((p) => p(ch))
          return negated ? !matched : matched
        }
        return { type: 'match', pred: combined }
      }
      let item
      if (source[pos] === '\\') {
        pos += 1
        item = readEscapeChar()
      } else {
        item = { kind: 'char', value: source[pos++] }
      }

      if (
        item.kind === 'char' &&
        pos + 1 < source.length &&
        source[pos] === '-' &&
        source[pos + 1] !== ']'
      ) {
        pos += 1 // consume '-'
        let endItem
        if (source[pos] === '\\') {
          pos += 1
          endItem = readEscapeChar()
        } else {
          endItem = { kind: 'char', value: source[pos++] }
        }
        if (endItem.kind !== 'char') fail('Invalid character class range')
        const startCode = item.value.charCodeAt(0)
        const endCode = endItem.value.charCodeAt(0)
        if (endCode < startCode) fail('Range out of order in character class')
        if (ignoreCase) {
          preds.push((ch) => {
            const c = ch.charCodeAt(0)
            if (c >= startCode && c <= endCode) return true
            const lc = ch.toLowerCase().charCodeAt(0)
            const uc = ch.toUpperCase().charCodeAt(0)
            return (lc >= startCode && lc <= endCode) || (uc >= startCode && uc <= endCode)
          })
        } else {
          preds.push((ch) => {
            const c = ch.charCodeAt(0)
            return c >= startCode && c <= endCode
          })
        }
      } else if (item.kind === 'class') {
        preds.push(item.pred)
      } else {
        const val = item.value
        if (ignoreCase) {
          const lower = val.toLowerCase()
          preds.push((ch) => ch.toLowerCase() === lower)
        } else {
          preds.push((ch) => ch === val)
        }
      }
    }
    fail('Unterminated character class')
  }

  function tryParseBraceQuantifier() {
    const saved = pos
    if (source[pos] !== '{') return null
    pos += 1
    const startMin = pos
    while (pos < source.length && isDigitChar(source[pos])) pos += 1
    if (pos === startMin) {
      pos = saved
      return null
    }
    const min = parseInt(source.slice(startMin, pos), 10)
    let max = min
    if (pos < source.length && source[pos] === ',') {
      pos += 1
      const startMax = pos
      while (pos < source.length && isDigitChar(source[pos])) pos += 1
      max = pos === startMax ? Infinity : parseInt(source.slice(startMax, pos), 10)
    }
    if (pos >= source.length || source[pos] !== '}') {
      pos = saved
      return null
    }
    pos += 1
    if (max < min) fail('Numbers out of order in {} quantifier')
    if (min > MAX_QUANTIFIER_REPEAT || (Number.isFinite(max) && max > MAX_QUANTIFIER_REPEAT)) {
      fail('Quantifier repetition count too large')
    }
    if (pos < source.length && source[pos] === '?') pos += 1
    return { min, max }
  }

  function parseAtom() {
    const ch = source[pos]
    if (ch === '(') {
      pos += 1
      if (source[pos] === '?') {
        pos += 1
        if (source[pos] === ':') {
          pos += 1
        } else if (source[pos] === '<') {
          const closeAngle = source.indexOf('>', pos + 1)
          if (closeAngle === -1) fail('Invalid group')
          pos = closeAngle + 1
        } else {
          fail('Unsupported group syntax')
        }
      }
      const inner = parseAlternation()
      if (pos >= source.length || source[pos] !== ')') fail('Unterminated group')
      pos += 1
      return inner
    }
    if (ch === '[') return parseCharClass()
    if (ch === '.') {
      pos += 1
      return { type: 'match', pred: dotAll ? () => true : (c) => !isLineBreak(c) }
    }
    if (ch === '^') {
      pos += 1
      return {
        type: 'assert',
        pred: (text, idx) => idx === 0 || (multiline && idx > 0 && isLineBreak(text[idx - 1])),
      }
    }
    if (ch === '$') {
      pos += 1
      return {
        type: 'assert',
        pred: (text, idx) => idx === text.length || (multiline && idx < text.length && isLineBreak(text[idx])),
      }
    }
    if (ch === '\\') {
      pos += 1
      if (pos >= source.length) fail('Trailing backslash')
      if (source[pos] === 'b') {
        pos += 1
        return {
          type: 'assert',
          pred: (text, idx) => {
            const before = idx > 0 && isWordChar(text[idx - 1])
            const after = idx < text.length && isWordChar(text[idx])
            return before !== after
          },
        }
      }
      if (source[pos] === 'B') {
        pos += 1
        return {
          type: 'assert',
          pred: (text, idx) => {
            const before = idx > 0 && isWordChar(text[idx - 1])
            const after = idx < text.length && isWordChar(text[idx])
            return before === after
          },
        }
      }
      const esc = readEscapeChar()
      if (esc.kind === 'class') return { type: 'match', pred: esc.pred }
      const val = esc.value
      if (ignoreCase) {
        const lower = val.toLowerCase()
        return { type: 'match', pred: (c) => c.toLowerCase() === lower }
      }
      return { type: 'match', pred: (c) => c === val }
    }
    if (ch === '*' || ch === '+' || ch === '?') {
      fail('Nothing to repeat')
    }
    pos += 1
    if (ignoreCase) {
      const lower = ch.toLowerCase()
      return { type: 'match', pred: (c) => c.toLowerCase() === lower }
    }
    return { type: 'match', pred: (c) => c === ch }
  }

  function parseSequence() {
    const items = []
    while (pos < source.length && source[pos] !== '|' && source[pos] !== ')') {
      const atom = parseAtom()
      let quant = null
      if (pos < source.length) {
        const qch = source[pos]
        if (qch === '*') {
          pos += 1
          if (pos < source.length && source[pos] === '?') pos += 1
          quant = { min: 0, max: Infinity }
        } else if (qch === '+') {
          pos += 1
          if (pos < source.length && source[pos] === '?') pos += 1
          quant = { min: 1, max: Infinity }
        } else if (qch === '?') {
          pos += 1
          if (pos < source.length && source[pos] === '?') pos += 1
          quant = { min: 0, max: 1 }
        } else if (qch === '{') {
          quant = tryParseBraceQuantifier()
        }
      }
      if (quant) {
        if (atom.type === 'assert') fail('Nothing to repeat')
        items.push({ type: 'repeat', node: atom, min: quant.min, max: quant.max })
      } else {
        items.push(atom)
      }
    }
    return items.length === 1 ? items[0] : { type: 'seq', items }
  }

  function parseAlternation() {
    const branches = [parseSequence()]
    while (pos < source.length && source[pos] === '|') {
      pos += 1
      branches.push(parseSequence())
    }
    return branches.length === 1 ? branches[0] : { type: 'alt', branches }
  }

  const ast = parseAlternation()
  if (pos < source.length) {
    fail(`Unexpected "${source[pos]}"`)
  }

  // Compile AST to Thompson NFA.
  const states = []
  function addNfaState(st) {
    if (states.length >= MAX_NFA_STATES) fail('Pattern too complex')
    states.push(st)
    return states.length - 1
  }

  function patch(outs, target) {
    for (const setter of outs) setter(target)
  }

  function compileNode(node) {
    if (node.type === 'match') {
      const st = { kind: 'match', pred: node.pred, out: -1 }
      const id = addNfaState(st)
      return { start: id, outs: [(t) => { st.out = t }] }
    }
    if (node.type === 'assert') {
      const st = { kind: 'assert', pred: node.pred, out: -1 }
      const id = addNfaState(st)
      return { start: id, outs: [(t) => { st.out = t }] }
    }
    if (node.type === 'seq') {
      if (node.items.length === 0) {
        const st = { kind: 'eps', out: -1 }
        const id = addNfaState(st)
        return { start: id, outs: [(t) => { st.out = t }] }
      }
      let cur = compileNode(node.items[0])
      for (let i = 1; i < node.items.length; i++) {
        const next = compileNode(node.items[i])
        patch(cur.outs, next.start)
        cur = { start: cur.start, outs: next.outs }
      }
      return cur
    }
    if (node.type === 'alt') {
      let cur = compileNode(node.branches[0])
      for (let i = 1; i < node.branches.length; i++) {
        const right = compileNode(node.branches[i])
        const sp = { kind: 'split', out1: cur.start, out2: right.start }
        const id = addNfaState(sp)
        cur = { start: id, outs: [...cur.outs, ...right.outs] }
      }
      return cur
    }
    if (node.type === 'repeat') {
      const seqParts = []
      for (let i = 0; i < node.min; i++) {
        seqParts.push(compileNode(node.node))
      }
      if (node.max === Infinity) {
        const body = compileNode(node.node)
        const sp = { kind: 'split', out1: body.start, out2: -1 }
        const id = addNfaState(sp)
        patch(body.outs, id)
        seqParts.push({ start: id, outs: [(t) => { sp.out2 = t }] })
      } else {
        for (let i = node.min; i < node.max; i++) {
          const body = compileNode(node.node)
          const sp = { kind: 'split', out1: body.start, out2: -1 }
          const id = addNfaState(sp)
          seqParts.push({ start: id, outs: [...body.outs, (t) => { sp.out2 = t }] })
        }
      }
      if (seqParts.length === 0) {
        const st = { kind: 'eps', out: -1 }
        const id = addNfaState(st)
        return { start: id, outs: [(t) => { st.out = t }] }
      }
      let cur = seqParts[0]
      for (let i = 1; i < seqParts.length; i++) {
        patch(cur.outs, seqParts[i].start)
        cur = { start: cur.start, outs: seqParts[i].outs }
      }
      return cur
    }
    fail('Invalid AST node')
  }

  const frag = compileNode(ast)
  const acceptId = addNfaState({ kind: 'accept' })
  patch(frag.outs, acceptId)
  const startId = frag.start

  const visited = new Int32Array(states.length)
  let stamp = 0

  function expand(initialIds, text, idx) {
    stamp += 1
    if (stamp > 0x70000000) {
      visited.fill(0)
      stamp = 1
    }
    const active = []
    const stack = [...initialIds]
    let accepted = false
    while (stack.length > 0) {
      const id = stack.pop()
      if (id < 0 || visited[id] === stamp) continue
      visited[id] = stamp
      const st = states[id]
      if (st.kind === 'accept') {
        accepted = true
      } else if (st.kind === 'eps') {
        stack.push(st.out)
      } else if (st.kind === 'split') {
        stack.push(st.out2, st.out1)
      } else if (st.kind === 'assert') {
        if (st.pred(text, idx)) stack.push(st.out)
      } else if (st.kind === 'match') {
        active.push(id)
      }
    }
    return { active, accepted }
  }

  return {
    source,
    flags,
    lastIndex: 0,
    test(input) {
      const text = String(input)
      const startPos = sticky ? Math.max(0, Math.min(this.lastIndex | 0, text.length)) : 0
      let step = expand([startId], text, startPos)
      if (step.accepted) return true
      for (let i = startPos; i < text.length; i++) {
        const ch = text[i]
        const nextSeeds = sticky ? [] : [startId]
        for (let k = 0; k < step.active.length; k++) {
          const st = states[step.active[k]]
          if (st.pred(ch)) nextSeeds.push(st.out)
        }
        if (nextSeeds.length === 0) return false
        step = expand(nextSeeds, text, i + 1)
        if (step.accepted) return true
      }
      return false
    },
  }
}

function normalizeReporters(raw, label) {
  let list
  if (typeof raw === 'string') {
    list = raw.split(',').map((s) => s.trim().toLowerCase()).filter(Boolean)
  } else if (Array.isArray(raw)) {
    list = raw.map((s) => String(s).trim().toLowerCase()).filter(Boolean)
  } else {
    throw new Error(`Invalid ${label}: expected string or string[]`)
  }
  if (!list.length) throw new Error(`Invalid ${label}: must specify at least one reporter`)
  for (const r of list) parseEnum(r, ['list', 'html', 'json'], label)
  return [...new Set(list)]
}

function parseShard(raw) {
  const m = /^(\d+)\/(\d+)$/.exec(String(raw).trim())
  if (!m) throw new Error(`Invalid --shard value "${raw}": expected k/n (e.g. 1/2)`)
  const index = Number(m[1])
  const total = Number(m[2])
  if (!Number.isInteger(index) || !Number.isInteger(total) || total < 1 || index < 1 || index > total) {
    throw new Error(`Invalid --shard value "${raw}": require 1 <= k <= n and n >= 1`)
  }
  return { index, total, raw: `${index}/${total}` }
}

function parsePositiveInt(raw, flag) {
  const n = Number(raw)
  if (!Number.isInteger(n) || n < 1) {
    throw new Error(`Invalid ${flag} value "${raw}": must be an integer >= 1`)
  }
  return n
}

function parseNonNegativeInt(raw, flag) {
  const n = Number(raw)
  if (!Number.isInteger(n) || n < 0) {
    throw new Error(`Invalid ${flag} value "${raw}": must be an integer >= 0`)
  }
  return n
}

function parseEnum(raw, allowed, flag) {
  if (!allowed.includes(raw)) {
    throw new Error(`Invalid ${flag} value "${raw}": expected ${allowed.join(' | ')}`)
  }
  return raw
}

function parseArgs(argv) {
  const opts = {
    targets: [],
    config: undefined,
    workers: undefined,
    shard: null,
    retries: undefined,
    grep: null,
    grepInvert: null,
    updateSnapshots: false,
    reporters: undefined,
    outputDir: undefined,
    reportDir: undefined,
    headed: undefined,
    screenshot: undefined,
    trace: undefined,
    video: undefined,
    timeout: undefined,
    maxBrowsers: undefined,
    help: false,
    version: false,
  }

  let endOfFlags = false
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]
    if (endOfFlags) {
      opts.targets.push(arg)
      continue
    }
    if (arg === '--') {
      endOfFlags = true
      continue
    }
    if (arg === '-h' || arg === '--help') {
      opts.help = true
      continue
    }
    if (arg === '-v' || arg === '--version') {
      opts.version = true
      continue
    }
    if (arg === '-u' || arg === '--update-snapshots') {
      opts.updateSnapshots = true
      continue
    }
    if (arg === '--headed') {
      opts.headed = true
      continue
    }

    const eqIdx = arg.indexOf('=')
    let flag = arg
    let val
    let hasInlineVal = false
    if (eqIdx !== -1 && arg.startsWith('--')) {
      flag = arg.slice(0, eqIdx)
      val = arg.slice(eqIdx + 1)
      hasInlineVal = true
    }

    const takeValue = () => {
      if (hasInlineVal) return val
      if (i + 1 >= argv.length) throw new Error(`Missing value for ${flag}`)
      i += 1
      return argv[i]
    }

    if (flag === '--config' || flag === '-c') {
      opts.config = takeValue()
    } else if (flag === '--workers' || flag === '-j') {
      opts.workers = parsePositiveInt(takeValue(), flag)
    } else if (flag === '--shard') {
      opts.shard = parseShard(takeValue())
    } else if (flag === '--retries') {
      opts.retries = parseNonNegativeInt(takeValue(), flag)
    } else if (flag === '--grep' || flag === '-g') {
      opts.grep = compileGrepMatcher(takeValue(), flag)
    } else if (flag === '--grep-invert') {
      opts.grepInvert = compileGrepMatcher(takeValue(), flag)
    } else if (flag === '--reporter') {
      opts.reporters = normalizeReporters(takeValue(), '--reporter')
    } else if (flag === '--output') {
      opts.outputDir = takeValue()
    } else if (flag === '--report-dir') {
      opts.reportDir = takeValue()
    } else if (flag === '--screenshot') {
      opts.screenshot = parseEnum(takeValue(), ['off', 'on', 'only-on-failure'], '--screenshot')
    } else if (flag === '--trace') {
      opts.trace = parseEnum(takeValue(), ['off', 'on', 'retain-on-failure'], '--trace')
    } else if (flag === '--video') {
      opts.video = parseEnum(takeValue(), ['off', 'on', 'retain-on-failure'], '--video')
    } else if (flag === '--timeout') {
      opts.timeout = parseNonNegativeInt(takeValue(), '--timeout')
    } else if (flag === '--max-browsers') {
      opts.maxBrowsers = parsePositiveInt(takeValue(), '--max-browsers')
    } else if (arg.startsWith('-')) {
      throw new Error(`Unknown option: ${arg}`)
    } else {
      opts.targets.push(arg)
    }
  }

  return opts
}

async function findConfigPath(cwd, explicitPath) {
  if (explicitPath !== undefined) {
    const abs = path.resolve(cwd, explicitPath)
    let stat
    try {
      stat = await fs.stat(abs)
    } catch {
      throw new Error(`Config file does not exist: ${explicitPath}`)
    }
    if (!stat.isFile()) {
      throw new Error(`Config path is not a file: ${explicitPath}`)
    }
    return abs
  }
  for (const name of CONFIG_FILENAMES) {
    const candidate = path.join(cwd, name)
    try {
      const stat = await fs.stat(candidate)
      if (stat.isFile()) return candidate
    } catch {}
  }
  return null
}

async function loadConfig(configPath) {
  if (!configPath) return { config: {}, configDir: null }
  const mod = await _internals.importFreshModule(configPath)
  const raw = mod?.default?.default ?? mod?.default ?? mod
  if (raw == null) return { config: {}, configDir: path.dirname(configPath) }
  if (typeof raw !== 'object' || Array.isArray(raw)) {
    throw new Error(`Invalid config in ${configPath}: default export must be an object`)
  }
  const config = {}
  if (raw.testDir !== undefined) {
    if (typeof raw.testDir !== 'string' || !raw.testDir) {
      throw new Error(`Invalid config.testDir in ${configPath}: must be a non-empty string`)
    }
    config.testDir = raw.testDir
  }
  if (raw.workers !== undefined) {
    config.workers = parsePositiveInt(raw.workers, 'config.workers')
  }
  if (raw.retries !== undefined) {
    config.retries = parseNonNegativeInt(raw.retries, 'config.retries')
  }
  if (raw.timeout !== undefined) {
    config.timeout = parseNonNegativeInt(raw.timeout, 'config.timeout')
  }
  if (raw.outputDir !== undefined) {
    if (typeof raw.outputDir !== 'string' || !raw.outputDir) {
      throw new Error(`Invalid config.outputDir in ${configPath}: must be a non-empty string`)
    }
    config.outputDir = raw.outputDir
  }
  if (raw.reportDir !== undefined) {
    if (typeof raw.reportDir !== 'string' || !raw.reportDir) {
      throw new Error(`Invalid config.reportDir in ${configPath}: must be a non-empty string`)
    }
    config.reportDir = raw.reportDir
  }
  if (raw.reporter !== undefined) {
    config.reporters = normalizeReporters(raw.reporter, 'config.reporter')
  }
  if (raw.use !== undefined) {
    if (!raw.use || typeof raw.use !== 'object' || Array.isArray(raw.use)) {
      throw new Error(`Invalid config.use in ${configPath}: must be an object`)
    }
    if (raw.use.screenshot !== undefined) {
      parseEnum(raw.use.screenshot, ['off', 'on', 'only-on-failure'], 'config.use.screenshot')
    }
    if (raw.use.trace !== undefined) {
      parseEnum(raw.use.trace, ['off', 'on', 'retain-on-failure'], 'config.use.trace')
    }
    if (raw.use.video !== undefined) {
      parseEnum(raw.use.video, ['off', 'on', 'retain-on-failure'], 'config.use.video')
    }
    config.use = { ...raw.use }
  }
  return { config, configDir: path.dirname(configPath) }
}

async function walkDir(dir, out) {
  const entries = await fs.readdir(dir, { withFileTypes: true })
  entries.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0))
  for (const entry of entries) {
    if (entry.isDirectory()) {
      if (IGNORE_DIRS.has(entry.name) || entry.name.startsWith('.')) continue
      await walkDir(path.join(dir, entry.name), out)
    } else if (entry.isFile() && TEST_FILE_RE.test(entry.name)) {
      out.push(path.join(dir, entry.name))
    }
  }
}

async function discoverTestFiles(targets, cwd) {
  const roots = targets.length ? targets : [cwd]
  const found = new Set()
  for (const rawTarget of roots) {
    const absTarget = path.resolve(cwd, rawTarget)
    let stat
    try {
      stat = await fs.stat(absTarget)
    } catch {
      throw new Error(`Test path does not exist: ${rawTarget}`)
    }
    if (stat.isFile()) {
      found.add(absTarget)
    } else if (stat.isDirectory()) {
      const files = []
      await walkDir(absTarget, files)
      for (const f of files) found.add(f)
    }
  }
  const relKey = (p) => path.relative(cwd, p).replace(/\\/g, '/')
  return [...found].sort((a, b) => {
    const ka = relKey(a)
    const kb = relKey(b)
    return ka < kb ? -1 : ka > kb ? 1 : 0
  })
}

function selectTests(fileEntries, opts) {
  const all = []
  for (const entry of fileEntries) {
    for (const t of entry.tests) {
      all.push({ entry, test: t })
    }
  }
  const hasOnly = all.some((x) => x.test.only)
  let filtered = hasOnly ? all.filter((x) => x.test.only) : all

  if (opts.grep) {
    filtered = filtered.filter((x) => {
      opts.grep.lastIndex = 0
      return opts.grep.test(x.test.fullTitle)
    })
  }
  if (opts.grepInvert) {
    filtered = filtered.filter((x) => {
      opts.grepInvert.lastIndex = 0
      return !opts.grepInvert.test(x.test.fullTitle)
    })
  }

  const totalAfterFilter = filtered.length
  if (opts.shard) {
    const { index, total } = opts.shard
    const start = Math.floor(((index - 1) * totalAfterFilter) / total)
    const end = Math.floor((index * totalAfterFilter) / total)
    filtered = filtered.slice(start, end)
  }

  return { selected: filtered, totalBeforeShard: totalAfterFilter }
}

function groupByFile(selected) {
  const map = new Map()
  for (const item of selected) {
    let bucket = map.get(item.entry.absPath)
    if (!bucket) {
      bucket = { entry: item.entry, absPath: item.entry.absPath, selectedIds: [] }
      map.set(item.entry.absPath, bucket)
    }
    bucket.selectedIds.push(item.test.id)
  }
  return [...map.values()]
}

function printLiveResult(result, index, total, enabled, stream = process.stdout) {
  if (!enabled) return
  const mark =
    result.status === 'passed'
      ? '✓'
      : result.status === 'flaky'
        ? '±'
        : result.status === 'skipped'
          ? '-'
          : '✘'
  const label = result.titlePath.join(' › ')
  const suffix = result.status === 'skipped' ? ' (skipped)' : ` (${formatDuration(result.durationMs)})`
  stream.write(`  ${mark} [${index}/${Math.max(total, index)}] ${label}${suffix}\n`)
}

function buildWorkerExecArgv(tasks) {
  const execArgv = [...process.execArgv]
  const hasTsTask = tasks.some((t) => _internals.isTypeScriptFile(t.absPath))
  if (hasTsTask && _internals.needsStripTypesFlag() && !execArgv.includes('--experimental-strip-types')) {
    execArgv.push('--experimental-strip-types')
  }
  return execArgv
}

function defaultPoolLaunchOptions(runOptions) {
  const cfgUse = runOptions?.configUse || {}
  const cliUse = runOptions?.cliUse || {}
  return {
    ...(cfgUse.launchOptions || {}),
    ...(cliUse.launchOptions || {}),
    ...(cfgUse.headless !== undefined ? { headless: cfgUse.headless } : {}),
    ...(cliUse.headless !== undefined ? { headless: cliUse.headless } : {}),
    ...(cfgUse.maxBrowsers !== undefined ? { maxBrowsers: cfgUse.maxBrowsers } : {}),
    ...(cliUse.maxBrowsers !== undefined ? { maxBrowsers: cliUse.maxBrowsers } : {}),
  }
}

async function runWithWorkers(tasks, workersCount, runOptions, cwd, totalSelected, listEnabled, listStream = process.stdout) {
  const allResults = []
  let completed = 0
  const queue = [...tasks]
  const poolCount = Math.min(workersCount, queue.length)
  const workerExecArgv = buildWorkerExecArgv(tasks)
  const activeWorkers = new Set()
  let firstError = null
  let aborting = false

  function abortSiblings(exceptEntry) {
    aborting = true
    queue.length = 0
    for (const entry of activeWorkers) {
      if (entry === exceptEntry) continue
      entry.abort()
    }
  }

  function spawnWorker() {
    return new Promise((resolve, reject) => {
      const child = fork(__filename, ['--__worker'], {
        cwd,
        env: { ...process.env },
        execArgv: workerExecArgv,
        stdio: ['inherit', 'inherit', 'inherit', 'ipc'],
      })
      let currentPromiseResolve = null
      let currentPromiseReject = null
      let normalShutdown = false
      let killTimer = null
      let settled = false

      const finishResolve = () => {
        if (settled) return
        settled = true
        if (killTimer) {
          clearTimeout(killTimer)
          killTimer = null
        }
        resolve()
      }

      const finishReject = (err) => {
        if (!firstError) firstError = err
        abortSiblings(entry)
        if (settled) return
        settled = true
        if (killTimer) {
          clearTimeout(killTimer)
          killTimer = null
        }
        reject(err)
      }

      const entry = {
        child,
        abort() {
          if (currentPromiseReject) {
            const rej = currentPromiseReject
            currentPromiseResolve = null
            currentPromiseReject = null
            rej(new Error('Worker aborted due to sibling failure'))
          }
          if (child.exitCode !== null || child.signalCode !== null) return
          try {
            if (child.connected) child.send({ type: 'abort' })
          } catch {}
          if (process.platform !== 'win32') {
            try {
              child.kill('SIGTERM')
            } catch {}
          }
          if (!killTimer) {
            killTimer = setTimeout(() => {
              try {
                if (child.exitCode === null && child.signalCode === null) {
                  child.kill('SIGKILL')
                }
              } catch {
                try {
                  child.kill()
                } catch {}
              }
            }, 3000)
            if (typeof killTimer.unref === 'function') killTimer.unref()
          }
        },
      }
      activeWorkers.add(entry)

      child.on('message', (msg) => {
        if (!msg || typeof msg !== 'object') return
        if (msg.type === 'result') {
          completed += 1
          allResults.push(msg.result)
          printLiveResult(msg.result, completed, totalSelected, listEnabled, listStream)
        } else if (msg.type === 'task-done') {
          if (currentPromiseResolve) {
            const res = currentPromiseResolve
            currentPromiseResolve = null
            currentPromiseReject = null
            res()
          }
        } else if (msg.type === 'task-error') {
          const err = new Error(msg.error || 'Worker task failed')
          if (currentPromiseReject) {
            const rej = currentPromiseReject
            currentPromiseResolve = null
            currentPromiseReject = null
            rej(err)
          } else {
            finishReject(err)
          }
        }
      })

      child.on('error', (err) => {
        if (currentPromiseReject) {
          const rej = currentPromiseReject
          currentPromiseResolve = null
          currentPromiseReject = null
          rej(err)
        }
        finishReject(err)
      })

      child.on('exit', (code, signal) => {
        activeWorkers.delete(entry)
        if (killTimer) {
          clearTimeout(killTimer)
          killTimer = null
        }
        if (aborting) {
          if (currentPromiseReject) {
            const rej = currentPromiseReject
            currentPromiseResolve = null
            currentPromiseReject = null
            rej(firstError || new Error('Worker aborted'))
          }
          finishResolve()
          return
        }
        if (signal != null || code !== 0) {
          const err =
            signal != null
              ? new Error(`Worker terminated by signal ${signal}`)
              : new Error(`Worker exited with code ${code}`)
          if (currentPromiseReject) {
            const rej = currentPromiseReject
            currentPromiseResolve = null
            currentPromiseReject = null
            rej(err)
          }
          finishReject(err)
          return
        }
        if (!normalShutdown) {
          const err = new Error('Worker exited unexpectedly before shutdown')
          if (currentPromiseReject) {
            const rej = currentPromiseReject
            currentPromiseResolve = null
            currentPromiseReject = null
            rej(err)
          }
          finishReject(err)
          return
        }
        finishResolve()
      })

      ;(async () => {
        try {
          while (!aborting && queue.length > 0) {
            const task = queue.shift()
            await new Promise((res, rej) => {
              currentPromiseResolve = res
              currentPromiseReject = rej
              child.send({
                type: 'run',
                absPath: task.absPath,
                selectedIds: task.selectedIds,
                runOptions,
                cwd,
              })
            })
          }
          if (!aborting) {
            normalShutdown = true
            if (child.connected) child.send({ type: 'shutdown' })
          }
        } catch (err) {
          if (!aborting) {
            if (!firstError) firstError = err
            abortSiblings(entry)
            entry.abort()
            if (child.exitCode !== null || child.signalCode !== null) {
              finishReject(err)
            }
          }
        }
      })()
    })
  }

  await Promise.allSettled(Array.from({ length: poolCount }, () => spawnWorker()))
  if (firstError) throw firstError
  return allResults
}

async function runWorkerProcess() {
  let pool = null
  let shuttingDown = false

  const cleanupAndExit = async (code) => {
    if (shuttingDown) return
    shuttingDown = true
    try {
      if (pool) await pool.closeAll()
    } catch {}
    process.exit(code)
  }

  process.on('disconnect', () => {
    cleanupAndExit(1)
  })
  process.on('SIGINT', () => {
    cleanupAndExit(130)
  })
  process.on('SIGTERM', () => {
    cleanupAndExit(143)
  })

  process.on('message', async (msg) => {
    if (!msg || typeof msg !== 'object') return
    if (msg.type === 'run') {
      try {
        if (!pool) {
          pool = _internals.createBrowserPool(defaultPoolLaunchOptions(msg.runOptions))
        }
        const entry = await _internals.loadTestFile(msg.absPath, msg.cwd)
        await _internals.runTestsForFile(entry, msg.selectedIds, pool, msg.runOptions, (res) => {
          if (process.connected) process.send({ type: 'result', result: res })
        })
        if (process.connected) process.send({ type: 'task-done' })
      } catch (err) {
        if (process.connected) {
          process.send({ type: 'task-error', error: err instanceof Error ? err.stack || err.message : String(err) })
        }
      }
    } else if (msg.type === 'shutdown') {
      await cleanupAndExit(0)
    } else if (msg.type === 'abort') {
      await cleanupAndExit(1)
    }
  })
}

async function runCli(argv = process.argv.slice(2), cwd = process.cwd()) {
  if (argv.includes('--__worker')) {
    await runWorkerProcess()
    return 0
  }

  let opts
  try {
    opts = parseArgs(argv)
  } catch (err) {
    process.stderr.write(`Error: ${err.message}\n`)
    return 1
  }

  if (opts.help) {
    printHelp()
    return 0
  }
  if (opts.version) {
    const pkg = require('./package.json')
    process.stdout.write(`${pkg.version}\n`)
    return 0
  }

  let configPath
  try {
    configPath = await findConfigPath(cwd, opts.config)
  } catch (err) {
    process.stderr.write(`Error: ${err.message}\n`)
    return 1
  }

  // On Node 22.6-22.17 invoked directly via CLI, re-exec with --experimental-strip-types
  // if config or explicit targets include TypeScript files.
  if (
    require.main === module &&
    _internals.needsStripTypesFlag() &&
    !process.env.FLUXWRIGHT_TS_REEXEC &&
    ((configPath && _internals.isTypeScriptFile(configPath)) ||
      opts.targets.some((t) => _internals.isTypeScriptFile(t)))
  ) {
    const child = spawnSync(
      process.execPath,
      ['--experimental-strip-types', ...process.execArgv, __filename, ...argv],
      { cwd, stdio: 'inherit', shell: false, env: { ...process.env, FLUXWRIGHT_TS_REEXEC: '1' } },
    )
    return child.signal ? 1 : (child.status ?? 1)
  }

  let config
  let configDir
  try {
    const loaded = await loadConfig(configPath)
    config = loaded.config
    configDir = loaded.configDir || cwd
  } catch (err) {
    process.stderr.write(`Error: ${err.message}\n`)
    return 1
  }

  const workers = opts.workers ?? config.workers ?? 1
  const retries = opts.retries ?? config.retries ?? 0
  const reporters = opts.reporters ?? config.reporters ?? ['list']
  const outputDir =
    opts.outputDir !== undefined
      ? path.resolve(cwd, opts.outputDir)
      : config.outputDir !== undefined
        ? path.resolve(configDir, config.outputDir)
        : path.resolve(cwd, 'test-results')
  const reportDir =
    opts.reportDir !== undefined
      ? opts.reportDir
      : config.reportDir !== undefined
        ? path.resolve(configDir, config.reportDir)
        : process.env.FLUXWRIGHT_HTML_REPORT || 'fluxwright-report'

  const effectiveTargets =
    opts.targets.length > 0
      ? opts.targets
      : config.testDir !== undefined
        ? [path.resolve(configDir, config.testDir)]
        : []

  const hadUpdateEnv = Object.prototype.hasOwnProperty.call(process.env, 'FLUXWRIGHT_UPDATE_SNAPSHOTS')
  const prevUpdateEnv = process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
  if (opts.updateSnapshots) {
    process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS = '1'
  }

  try {
    const startedAt = new Date().toISOString()
    const startMs = Date.now()

    let files
    try {
      files = await discoverTestFiles(effectiveTargets, cwd)
    } catch (err) {
      process.stderr.write(`Error: ${err.message}\n`)
      return 1
    }

    if (files.length === 0) {
      process.stderr.write('No test files found.\n')
      return 1
    }

    if (
      require.main === module &&
      _internals.needsStripTypesFlag() &&
      !process.env.FLUXWRIGHT_TS_REEXEC &&
      files.some((f) => _internals.isTypeScriptFile(f))
    ) {
      const child = spawnSync(
        process.execPath,
        ['--experimental-strip-types', ...process.execArgv, __filename, ...argv],
        { cwd, stdio: 'inherit', shell: false, env: { ...process.env, FLUXWRIGHT_TS_REEXEC: '1' } },
      )
      return child.signal ? 1 : (child.status ?? 1)
    }

    const fileEntries = []
    for (const file of files) {
      fileEntries.push(await _internals.loadTestFile(file, cwd))
    }

    const { selected } = selectTests(fileEntries, opts)
    const tasks = groupByFile(selected)
    const listEnabled = reporters.includes('list')
    const jsonEnabled = reporters.includes('json')
    const listStream = jsonEnabled ? process.stderr : process.stdout

    const configUse = {
      ...(config.timeout !== undefined ? { timeout: config.timeout } : {}),
      ...(config.use || {}),
    }
    const cliUse = {}
    if (opts.screenshot !== undefined) cliUse.screenshot = opts.screenshot
    if (opts.trace !== undefined) cliUse.trace = opts.trace
    if (opts.video !== undefined) cliUse.video = opts.video
    if (opts.headed === true) cliUse.headless = false
    if (opts.timeout !== undefined) cliUse.timeout = opts.timeout
    if (opts.maxBrowsers !== undefined) cliUse.maxBrowsers = opts.maxBrowsers

    const runOptions = {
      retries,
      outputDir,
      configUse,
      cliUse,
    }

    let results = []
    if (tasks.length > 0) {
      if (workers > 1 && tasks.length > 1) {
        results = await runWithWorkers(tasks, workers, runOptions, cwd, selected.length, listEnabled, listStream)
      } else {
        const pool = _internals.createBrowserPool(defaultPoolLaunchOptions(runOptions))
        let completed = 0
        try {
          for (const task of tasks) {
            const fileResults = await _internals.runTestsForFile(
              task.entry,
              task.selectedIds,
              pool,
              runOptions,
              (res) => {
                completed += 1
                printLiveResult(res, completed, selected.length, listEnabled, listStream)
              },
            )
            results.push(...fileResults)
          }
        } finally {
          await pool.closeAll()
        }
      }
    }

    const durationMs = Date.now() - startMs
    const passedCount = results.filter((r) => r.status === 'passed').length
    const flakyCount = results.filter((r) => r.status === 'flaky').length
    const failedCount = results.filter((r) => r.status === 'failed' || r.status === 'timedOut').length
    const skippedCount = results.filter((r) => r.status === 'skipped').length

    const report = {
      startedAt,
      durationMs,
      workers,
      shard: opts.shard ? opts.shard.raw : null,
      results,
    }

    if (listEnabled) {
      const failures = results.filter((r) => r.status === 'failed' || r.status === 'timedOut')
      if (failures.length > 0) {
        listStream.write('\nFailures:\n')
        failures.forEach((f, idx) => {
          listStream.write(`\n  ${idx + 1}) ${f.titlePath.join(' › ')}\n`)
          if (f.error) {
            listStream.write(`     ${f.error.stack || f.error.message}\n`)
          }
          for (const att of f.attachments || []) {
            if (att.path) listStream.write(`     attachment (${att.name}): ${att.path}\n`)
          }
        })
      }
      const parts = [
        `${passedCount} passed`,
        flakyCount ? `${flakyCount} flaky` : null,
        failedCount ? `${failedCount} failed` : null,
        skippedCount ? `${skippedCount} skipped` : null,
      ].filter(Boolean)
      listStream.write(`\n${parts.join(', ')} (${formatDuration(durationMs)})\n`)
    }

    if (reporters.includes('html')) {
      const reportPath = reportDir.endsWith('.html')
        ? path.resolve(cwd, reportDir)
        : path.resolve(cwd, reportDir, 'index.html')
      const written = await writeHtmlReport(report, reportPath)
      if (listEnabled) {
        listStream.write(`HTML report: ${written}\n`)
      }
    }

    if (jsonEnabled) {
      process.stdout.write(`${JSON.stringify(report, null, 2)}\n`)
    }

    return failedCount > 0 ? 1 : 0
  } finally {
    if (opts.updateSnapshots) {
      if (hadUpdateEnv) process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS = prevUpdateEnv
      else delete process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
    }
  }
}

if (require.main === module) {
  runCli().then(
    (code) => {
      if (!process.argv.includes('--__worker')) process.exit(code)
    },
    (err) => {
      process.stderr.write(`${err instanceof Error ? err.stack || err.message : String(err)}\n`)
      process.exit(1)
    },
  )
}

module.exports = {
  compileGrepMatcher,
  discoverTestFiles,
  findConfigPath,
  loadConfig,
  parseArgs,
  runCli,
  selectTests,
}
