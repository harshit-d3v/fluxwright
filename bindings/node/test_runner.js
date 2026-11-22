'use strict'

const assert = require('node:assert/strict')
const crypto = require('node:crypto')
const fs = require('node:fs/promises')
const path = require('node:path')
const { pathToFileURL } = require('node:url')
const { isDeepStrictEqual } = require('node:util')
const fluxwright = require('./addon.js')
const { setSoftErrorCollector } = require('./expect.js')

const DEFAULT_TEST_TIMEOUT_MS = 30_000

const LAUNCH_OPTION_KEYS = new Set([
  'maxBrowsers',
  'executablePath',
  'headless',
  'queueTimeout',
  'memoryCeilingMb',
  'maxConcurrencyPerHost',
  'minDelayPerHost',
  'args',
  'ignoreDefaultArgs',
  'channel',
  'prewarmContexts',
])

const CONTEXT_OPTION_KEYS = new Set([
  'proxy',
  'userAgent',
  'locale',
  'timezoneId',
  'geolocation',
  'permissions',
  'viewport',
  'deviceScaleFactor',
  'colorScheme',
  'isMobile',
  'hasTouch',
  'extraHTTPHeaders',
  'ignoreHTTPSErrors',
  'javaScriptEnabled',
  'offline',
  'baseURL',
  'httpCredentials',
  'bypassCSP',
  'storageState',
  'recordHar',
  'recordVideo',
  'resourcePreset',
  'blockResources',
  'blockUrlPatterns',
])

class SkipSignal extends Error {
  constructor(reason = 'Skipped') {
    super(reason)
    this.name = 'SkipSignal'
  }
}

function formatVal(v) {
  try {
    if (v instanceof RegExp) return String(v)
    if (typeof v === 'undefined') return 'undefined'
    if (typeof v === 'function') return `[Function ${v.name || 'anonymous'}]`
    return JSON.stringify(v)
  } catch {
    return String(v)
  }
}

class ValueAssertions {
  #actual
  #negated
  #onSoftError

  constructor(actual, negated = false, onSoftError = null) {
    this.#actual = actual
    this.#negated = negated
    this.#onSoftError = onSoftError
  }

  get not() {
    return new ValueAssertions(this.#actual, !this.#negated, this.#onSoftError)
  }

  #assert(pass, matcher, expectedDesc, actualDesc = formatVal(this.#actual)) {
    const ok = this.#negated ? !pass : pass
    if (ok) return
    const prefix = this.#negated ? 'not ' : ''
    const err = new assert.AssertionError({
      message: `expect(received).${this.#negated ? 'not.' : ''}${matcher}\nExpected: ${prefix}${expectedDesc}\nReceived: ${actualDesc}`,
      actual: this.#actual,
      expected: expectedDesc,
      operator: matcher,
    })
    if (this.#onSoftError) {
      this.#onSoftError(err)
      return
    }
    throw err
  }

  toBe(expected) {
    this.#assert(Object.is(this.#actual, expected), 'toBe', formatVal(expected))
  }

  toEqual(expected) {
    this.#assert(isDeepStrictEqual(this.#actual, expected), 'toEqual', formatVal(expected))
  }

  toStrictEqual(expected) {
    this.#assert(isDeepStrictEqual(this.#actual, expected), 'toStrictEqual', formatVal(expected))
  }

  toContain(expected) {
    let has = false
    if (typeof this.#actual === 'string') {
      has = this.#actual.includes(String(expected))
    } else if (Array.isArray(this.#actual)) {
      has = this.#actual.includes(expected)
    } else if (this.#actual instanceof Set) {
      has = this.#actual.has(expected)
    } else if (this.#actual && typeof this.#actual[Symbol.iterator] === 'function') {
      has = [...this.#actual].includes(expected)
    }
    this.#assert(has, 'toContain', formatVal(expected))
  }

  toMatch(pattern) {
    let ok
    if (pattern instanceof RegExp) {
      const re = new RegExp(pattern.source, pattern.flags.replace(/g/g, ''))
      re.lastIndex = 0
      ok = re.test(String(this.#actual))
    } else {
      ok = String(this.#actual).includes(String(pattern))
    }
    this.#assert(ok, 'toMatch', formatVal(pattern))
  }

  toBeTruthy() {
    this.#assert(Boolean(this.#actual), 'toBeTruthy', 'truthy')
  }

  toBeFalsy() {
    this.#assert(!this.#actual, 'toBeFalsy', 'falsy')
  }

  toBeNull() {
    this.#assert(this.#actual === null, 'toBeNull', 'null')
  }

  toBeUndefined() {
    this.#assert(this.#actual === undefined, 'toBeUndefined', 'undefined')
  }

  toBeDefined() {
    this.#assert(this.#actual !== undefined, 'toBeDefined', 'defined')
  }

  toBeGreaterThan(n) {
    this.#assert(typeof this.#actual === 'number' && this.#actual > n, 'toBeGreaterThan', `> ${n}`)
  }

  toBeGreaterThanOrEqual(n) {
    this.#assert(typeof this.#actual === 'number' && this.#actual >= n, 'toBeGreaterThanOrEqual', `>= ${n}`)
  }

  toBeLessThan(n) {
    this.#assert(typeof this.#actual === 'number' && this.#actual < n, 'toBeLessThan', `< ${n}`)
  }

  toBeLessThanOrEqual(n) {
    this.#assert(typeof this.#actual === 'number' && this.#actual <= n, 'toBeLessThanOrEqual', `<= ${n}`)
  }

  toBeCloseTo(n, numDigits = 2) {
    const pass =
      typeof this.#actual === 'number' &&
      Math.abs(this.#actual - n) < Math.pow(10, -numDigits) / 2
    this.#assert(pass, 'toBeCloseTo', `${n} (precision ${numDigits})`)
  }

  toHaveLength(len) {
    const actualLen = this.#actual != null ? this.#actual.length ?? this.#actual.size : undefined
    this.#assert(actualLen === len, 'toHaveLength', `length ${len}`, `length ${actualLen}`)
  }

  toHaveProperty(keyPath, expectedValue) {
    const parts = Array.isArray(keyPath) ? keyPath : String(keyPath).split('.')
    let cur = this.#actual
    let exists = true
    for (const part of parts) {
      if (cur == null || (typeof cur !== 'object' && typeof cur !== 'function') || !(part in Object(cur))) {
        exists = false
        cur = undefined
        break
      }
      cur = cur[part]
    }
    const hasExpected = arguments.length >= 2
    const pass = exists && (!hasExpected || isDeepStrictEqual(cur, expectedValue))
    this.#assert(pass, 'toHaveProperty', hasExpected ? `${parts.join('.')} = ${formatVal(expectedValue)}` : parts.join('.'))
  }

  toThrow(expected) {
    if (typeof this.#actual !== 'function') {
      throw new TypeError('expect(fn).toThrow: received value must be a function')
    }
    let threw = false
    let caught
    try {
      this.#actual()
    } catch (err) {
      threw = true
      caught = err
    }
    let pass = threw
    if (pass && expected !== undefined) {
      const msg = caught instanceof Error ? caught.message : String(caught)
      if (expected instanceof RegExp) {
        const re = new RegExp(expected.source, expected.flags.replace(/g/g, ''))
        re.lastIndex = 0
        pass = re.test(msg)
      } else if (typeof expected === 'string') {
        pass = msg.includes(expected)
      } else if (typeof expected === 'function') {
        pass = caught instanceof expected
      }
    }
    this.#assert(pass, 'toThrow', expected === undefined ? 'error' : formatVal(expected), threw ? String(caught) : 'did not throw')
  }
}

function wrapExpect(baseExpect) {
  const recordSoftValueError = (err) => {
    if (state.activeSoftErrors) {
      state.activeSoftErrors.push(err)
    }
  }

  function expect(target) {
    if (target != null && (typeof target === 'object' || typeof target === 'function')) {
      try {
        return baseExpect(target)
      } catch (err) {
        if (!(err instanceof TypeError) || err.message !== 'expect: argument must be a Page or Locator') {
          throw err
        }
      }
    }
    return new ValueAssertions(target, false, null)
  }

  expect.soft = (target) => {
    if (target != null && (typeof target === 'object' || typeof target === 'function')) {
      try {
        return baseExpect.soft(target)
      } catch (err) {
        if (!(err instanceof TypeError) || err.message !== 'expect.soft: argument must be a Page or Locator') {
          throw err
        }
      }
    }
    return new ValueAssertions(target, false, recordSoftValueError)
  }

  expect.configure = (options) => wrapExpect(baseExpect.configure(options))
  return expect
}

const expect = wrapExpect(fluxwright.expect)

function createSuite(title, parent = null, file = null) {
  return {
    title,
    parent,
    file,
    tests: [],
    suites: [],
    beforeEach: [],
    afterEach: [],
    beforeAll: [],
    afterAll: [],
    useOptions: {},
    only: false,
    skipReason: null,
  }
}

function mergeUseOptions(base, extra) {
  if (!extra || typeof extra !== 'object') return { ...base }
  const out = { ...base }
  for (const [k, v] of Object.entries(extra)) {
    if (v !== undefined) out[k] = v
  }
  if (base.launchOptions || extra.launchOptions) {
    out.launchOptions = { ...base.launchOptions, ...extra.launchOptions }
  }
  if (base.extraHTTPHeaders || extra.extraHTTPHeaders) {
    out.extraHTTPHeaders = { ...base.extraHTTPHeaders, ...extra.extraHTTPHeaders }
  }
  return out
}

const state = {
  rootSuite: null,
  suiteStack: [],
  activeTestInfo: null,
  activeSoftErrors: null,
  nextId: 1,
}

function currentSuite() {
  if (state.suiteStack.length > 0) {
    return state.suiteStack[state.suiteStack.length - 1]
  }
  if (!state.rootSuite) {
    state.rootSuite = createSuite('', null, null)
  }
  return state.rootSuite
}

function registerTest(title, fn, opts = {}) {
  if (typeof title !== 'string' || !title) {
    throw new TypeError('test: title must be a non-empty string')
  }
  if (typeof fn !== 'function' && !opts.skipped) {
    throw new TypeError('test: test body must be a function')
  }
  const suite = currentSuite()
  suite.tests.push({
    id: `t-${state.nextId++}`,
    title,
    fn: fn || (() => {}),
    suite,
    only: Boolean(opts.only),
    skipped: Boolean(opts.skipped),
    skipReason: opts.skipReason || null,
  })
}

function test(title, fn) {
  registerTest(title, fn)
}

test.only = function (title, fn) {
  registerTest(title, fn, { only: true })
}

test.skip = function (titleOrCondition, fnOrReason) {
  if (typeof titleOrCondition === 'string') {
    registerTest(titleOrCondition, typeof fnOrReason === 'function' ? fnOrReason : () => {}, {
      skipped: true,
      skipReason: typeof fnOrReason === 'string' ? fnOrReason : 'Skipped',
    })
    return
  }
  const shouldSkip = arguments.length === 0 ? true : Boolean(titleOrCondition)
  const reason = typeof fnOrReason === 'string' ? fnOrReason : 'Skipped'
  if (state.activeTestInfo) {
    if (shouldSkip) throw new SkipSignal(reason)
    return
  }
  if (shouldSkip) {
    currentSuite().skipReason = reason
  }
}

function registerDescribe(title, fn, opts = {}) {
  if (typeof title !== 'string') {
    throw new TypeError('test.describe: title must be a string')
  }
  if (typeof fn !== 'function') {
    throw new TypeError('test.describe: callback must be a function')
  }
  const parent = currentSuite()
  const suite = createSuite(title, parent, parent.file)
  suite.only = Boolean(opts.only)
  suite.skipReason = opts.skipReason || null
  parent.suites.push(suite)
  state.suiteStack.push(suite)
  try {
    fn()
  } finally {
    state.suiteStack.pop()
  }
}

test.describe = function (title, fn) {
  registerDescribe(title, fn)
}

test.describe.only = function (title, fn) {
  registerDescribe(title, fn, { only: true })
}

test.describe.skip = function (title, fn) {
  registerDescribe(title, fn, { skipReason: 'Skipped' })
}

test.beforeEach = function (fn) {
  if (typeof fn !== 'function') throw new TypeError('test.beforeEach: hook must be a function')
  currentSuite().beforeEach.push(fn)
}

test.afterEach = function (fn) {
  if (typeof fn !== 'function') throw new TypeError('test.afterEach: hook must be a function')
  currentSuite().afterEach.push(fn)
}

test.beforeAll = function (fn) {
  if (typeof fn !== 'function') throw new TypeError('test.beforeAll: hook must be a function')
  currentSuite().beforeAll.push(fn)
}

test.afterAll = function (fn) {
  if (typeof fn !== 'function') throw new TypeError('test.afterAll: hook must be a function')
  currentSuite().afterAll.push(fn)
}

test.use = function (options) {
  if (!options || typeof options !== 'object') {
    throw new TypeError('test.use: options must be an object')
  }
  const suite = currentSuite()
  suite.useOptions = mergeUseOptions(suite.useOptions, options)
}

test.info = function () {
  if (!state.activeTestInfo) {
    throw new Error('test.info() can only be called during a running test')
  }
  return state.activeTestInfo
}

test.expect = expect

function defineConfig(config) {
  return config
}

function stripComments(src) {
  let out = ''
  let i = 0
  const n = src.length
  const templateBraceStack = []

  while (i < n) {
    const ch = src[i]
    const next = i + 1 < n ? src[i + 1] : ''

    if (ch === "'" || ch === '"') {
      const quote = ch
      out += ch
      i += 1
      while (i < n) {
        const c = src[i]
        out += c
        i += 1
        if (c === '\\' && i < n) {
          out += src[i]
          i += 1
        } else if (c === quote) {
          break
        }
      }
      continue
    }

    if (ch === '`') {
      out += ch
      i += 1
      while (i < n) {
        const c = src[i]
        if (c === '\\' && i + 1 < n) {
          out += c + src[i + 1]
          i += 2
          continue
        }
        if (c === '$' && i + 1 < n && src[i + 1] === '{') {
          out += '${'
          i += 2
          templateBraceStack.push(1)
          break
        }
        out += c
        i += 1
        if (c === '`') break
      }
      continue
    }

    if (templateBraceStack.length > 0) {
      if (ch === '{') {
        templateBraceStack[templateBraceStack.length - 1] += 1
      } else if (ch === '}') {
        templateBraceStack[templateBraceStack.length - 1] -= 1
        if (templateBraceStack[templateBraceStack.length - 1] === 0) {
          templateBraceStack.pop()
          out += '}'
          i += 1
          while (i < n) {
            const c = src[i]
            if (c === '\\' && i + 1 < n) {
              out += c + src[i + 1]
              i += 2
              continue
            }
            if (c === '$' && i + 1 < n && src[i + 1] === '{') {
              out += '${'
              i += 2
              templateBraceStack.push(1)
              break
            }
            out += c
            i += 1
            if (c === '`') break
          }
          continue
        }
      }
    }

    if (ch === '/' && next === '/') {
      i += 2
      while (i < n && src[i] !== '\n') i += 1
      continue
    }

    if (ch === '/' && next === '*') {
      i += 2
      while (i + 1 < n && !(src[i] === '*' && src[i + 1] === '/')) i += 1
      i = i + 1 < n ? i + 2 : n
      out += ' '
      continue
    }

    out += ch
    i += 1
  }
  return out
}

function skipStringLiteral(text, startIdx) {
  const quote = text[startIdx]
  let i = startIdx + 1
  const n = text.length
  if (quote === "'" || quote === '"') {
    while (i < n) {
      if (text[i] === '\\') {
        i += 2
        continue
      }
      if (text[i] === quote) return i
      i += 1
    }
    return n - 1
  }
  if (quote === '`') {
    let exprDepth = 0
    while (i < n) {
      if (text[i] === '\\') {
        i += 2
        continue
      }
      if (exprDepth === 0) {
        if (text[i] === '`') return i
        if (text[i] === '$' && i + 1 < n && text[i + 1] === '{') {
          exprDepth = 1
          i += 2
          continue
        }
      } else {
        if (text[i] === "'" || text[i] === '"' || text[i] === '`') {
          i = skipStringLiteral(text, i) + 1
          continue
        }
        if (text[i] === '{') exprDepth += 1
        else if (text[i] === '}') exprDepth -= 1
      }
      i += 1
    }
    return n - 1
  }
  return startIdx
}

function requestedFixtures(fn) {
  const result = new Set()
  if (typeof fn !== 'function') return result
  const src = stripComments(fn.toString()).trim()
  const arrowIdx = src.indexOf('=>')
  const openParen = src.indexOf('(')
  let firstParam = ''

  if (openParen !== -1 && (arrowIdx === -1 || openParen < arrowIdx)) {
    let depth = 0
    let closeParen = -1
    for (let i = openParen; i < src.length; i++) {
      const ch = src[i]
      if (ch === "'" || ch === '"' || ch === '`') {
        i = skipStringLiteral(src, i)
        continue
      }
      if (ch === '(') depth += 1
      else if (ch === ')') {
        depth -= 1
        if (depth === 0) {
          closeParen = i
          break
        }
      }
    }
    if (closeParen !== -1) {
      const paramsText = src.slice(openParen + 1, closeParen).trim()
      if (!paramsText) return result
      // Extract only the first top-level parameter.
      let bDepth = 0
      let pDepth = 0
      let sDepth = 0
      let end = paramsText.length
      for (let i = 0; i < paramsText.length; i++) {
        const c = paramsText[i]
        if (c === "'" || c === '"' || c === '`') {
          i = skipStringLiteral(paramsText, i)
          continue
        }
        if (c === '{') bDepth += 1
        else if (c === '}') bDepth -= 1
        else if (c === '(') pDepth += 1
        else if (c === ')') pDepth -= 1
        else if (c === '[') sDepth += 1
        else if (c === ']') sDepth -= 1
        else if (c === ',' && bDepth === 0 && pDepth === 0 && sDepth === 0) {
          end = i
          break
        }
      }
      firstParam = paramsText.slice(0, end).trim()
    }
  } else if (arrowIdx !== -1) {
    firstParam = src.slice(0, arrowIdx).trim().replace(/^async\s+/, '').trim()
  }

  if (!firstParam) return result
  if (firstParam.startsWith('{')) {
    let braceDepth = 0
    let closeBrace = -1
    for (let i = 0; i < firstParam.length; i++) {
      const c = firstParam[i]
      if (c === "'" || c === '"' || c === '`') {
        i = skipStringLiteral(firstParam, i)
        continue
      }
      if (c === '{') braceDepth += 1
      else if (c === '}') {
        braceDepth -= 1
        if (braceDepth === 0) {
          closeBrace = i
          break
        }
      }
    }
    const inner = closeBrace !== -1 ? firstParam.slice(1, closeBrace) : firstParam.slice(1)
    let depth = 0
    let tokenStart = 0
    const tokens = []
    for (let i = 0; i < inner.length; i++) {
      const c = inner[i]
      if (c === "'" || c === '"' || c === '`') {
        i = skipStringLiteral(inner, i)
        continue
      }
      if (c === '{' || c === '[' || c === '(') depth += 1
      else if (c === '}' || c === ']' || c === ')') depth -= 1
      else if (c === ',' && depth === 0) {
        tokens.push(inner.slice(tokenStart, i))
        tokenStart = i + 1
      }
    }
    tokens.push(inner.slice(tokenStart))
    for (const tok of tokens) {
      const keyMatch = /^\s*([A-Za-z_$][A-Za-z0-9_$]*)/.exec(tok)
      if (keyMatch) result.add(keyMatch[1])
    }
    return result
  }

  // Non-destructured parameter like `async (fixtures) => ...`
  result.add('browser')
  result.add('context')
  result.add('page')
  result.add('baseURL')
  return result
}

const WINDOWS_RESERVED_RE = /^(?:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)/i

function trimEdgeChars(str, leadingSet, trailingSet) {
  let start = 0
  let end = str.length
  while (start < end && leadingSet.has(str[start])) start += 1
  while (end > start && trailingSet.has(str[end - 1])) end -= 1
  return str.slice(start, end)
}

const TRIM_LEADING_SEGMENT = new Set(['-', ' ', '\t', '\r', '\n'])
const TRIM_TRAILING_SEGMENT = new Set(['-', '.', ' ', '\t', '\r', '\n'])
const TRIM_TRAILING_PREFIX = new Set(['-', '.'])

function sanitizePathSegment(name) {
  const raw = String(name ?? '')
  if (!raw) return 'test'
  const hash = crypto.createHash('sha1').update(raw).digest('hex').slice(0, 8)
  let cleaned = trimEdgeChars(
    raw.replace(/[^a-zA-Z0-9._-]+/g, '-').replace(/-+/g, '-'),
    TRIM_LEADING_SEGMENT,
    TRIM_TRAILING_SEGMENT,
  )
  let modified = cleaned !== raw
  if (!cleaned || cleaned === '.' || cleaned === '..') {
    cleaned = 'test'
    modified = true
  }
  if (WINDOWS_RESERVED_RE.test(cleaned)) {
    cleaned = `_${cleaned}`
    modified = true
  }
  if (cleaned.length > 100) {
    const prefix = trimEdgeChars(cleaned.slice(0, 91), TRIM_TRAILING_PREFIX, TRIM_TRAILING_PREFIX) || 'test'
    return `${prefix}-${hash}`
  }
  if (modified) {
    const maxPrefix = 100 - 1 - hash.length
    const prefix =
      cleaned.length > maxPrefix
        ? trimEdgeChars(cleaned.slice(0, maxPrefix), TRIM_TRAILING_PREFIX, TRIM_TRAILING_PREFIX) || 'test'
        : cleaned
    return `${prefix}-${hash}`
  }
  return cleaned
}

function pickKeys(obj, allowed) {
  const out = {}
  if (!obj || typeof obj !== 'object') return out
  for (const key of Object.keys(obj)) {
    if (allowed.has(key) && obj[key] !== undefined) {
      out[key] = obj[key]
    }
  }
  return out
}

function createBrowserPool(defaultLaunchOptions = {}) {
  const entries = new Map()
  let launchCount = 0

  async function getBrowser(overrideLaunchOptions = {}) {
    const merged = pickKeys({ ...defaultLaunchOptions, ...overrideLaunchOptions }, LAUNCH_OPTION_KEYS)
    if (merged.headless === undefined) merged.headless = true
    const key = JSON.stringify(Object.keys(merged).sort().map((k) => [k, merged[k]]))
    let entry = entries.get(key)
    if (!entry) {
      const promise = fluxwright.chromium.launch(merged).then((browser) => {
        launchCount += 1
        return browser
      })
      entry = { promise, browser: null }
      entries.set(key, entry)
      try {
        entry.browser = await promise
      } catch (err) {
        entries.delete(key)
        throw err
      }
    }
    return entry.browser || entry.promise
  }

  async function closeAll() {
    const closing = []
    for (const entry of entries.values()) {
      closing.push(
        Promise.resolve(entry.browser || entry.promise)
          .then((b) => b.close())
          .catch(() => {}),
      )
    }
    entries.clear()
    await Promise.allSettled(closing)
  }

  return {
    getBrowser,
    closeAll,
    getLaunchCount: () => launchCount,
  }
}

let recordVideoSupported = true

async function openContextWithFeatureDetection(browser, contextOptions, videoConfig) {
  if (videoConfig && recordVideoSupported) {
    try {
      return await browser.newContext({ ...contextOptions, recordVideo: videoConfig })
    } catch (err) {
      if (err instanceof TypeError && /recordVideo.*not supported/i.test(err.message)) {
        recordVideoSupported = false
      } else {
        throw err
      }
    }
  }
  return browser.newContext(contextOptions)
}

async function runWithTimeout(fn, timeoutMs, label) {
  if (!timeoutMs || timeoutMs <= 0) return fn()
  let timer = null
  try {
    return await Promise.race([
      Promise.resolve().then(fn),
      new Promise((_, reject) => {
        timer = setTimeout(() => {
          const err = new Error(`Test timeout of ${timeoutMs}ms exceeded${label ? ` (${label})` : ''}.`)
          err.name = 'TimeoutError'
          reject(err)
        }, timeoutMs)
      }),
    ])
  } finally {
    if (timer) clearTimeout(timer)
  }
}

function collectSuiteAncestors(suite) {
  const chain = []
  let cur = suite
  while (cur) {
    chain.push(cur)
    cur = cur.parent
  }
  return chain.reverse()
}

function flattenSuiteTests(rootSuite, relFile) {
  const out = []
  function walk(suite, titleParts, inheritedOnly, inheritedSkip) {
    const only = inheritedOnly || suite.only
    const skipReason = inheritedSkip || suite.skipReason
    for (const item of suite.tests) {
      const titlePath = [...titleParts, item.title]
      out.push({
        id: item.id,
        title: item.title,
        titlePath,
        fullTitle: titlePath.join(' › '),
        file: relFile,
        fn: item.fn,
        suite: item.suite,
        only: only || item.only,
        skipped: Boolean(skipReason || item.skipped),
        skipReason: skipReason || item.skipReason || null,
      })
    }
    for (const child of suite.suites) {
      const nextParts = child.title ? [...titleParts, child.title] : titleParts
      walk(child, nextParts, only, skipReason)
    }
  }
  const initialParts = relFile ? [relFile] : []
  walk(rootSuite, initialParts, false, null)
  return out
}

let loadSeq = 0

function parseNodeVersion(v = process.versions.node) {
  const [major = 0, minor = 0, patch = 0] = String(v).split('.').map((n) => parseInt(n, 10) || 0)
  return { major, minor, patch }
}

function isTypeScriptFile(filePath) {
  return /\.(?:ts|mts|cts)$/i.test(String(filePath))
}

function supportsNativeTypeScript(version = process.versions.node) {
  const { major, minor } = parseNodeVersion(version)
  return major > 22 || (major === 22 && minor >= 6)
}

function needsStripTypesFlag(version = process.versions.node) {
  const { major, minor } = parseNodeVersion(version)
  if (major === 22 && minor >= 6 && minor < 18) {
    return !process.features?.typescript && !process.execArgv.includes('--experimental-strip-types')
  }
  return false
}

async function importFreshModule(absPath) {
  if (isTypeScriptFile(absPath) && !supportsNativeTypeScript()) {
    throw new Error(
      `Cannot load TypeScript file "${absPath}" on Node.js ${process.versions.node}: native TypeScript support requires Node.js >= 22.6.0.`,
    )
  }
  try {
    delete require.cache[absPath]
  } catch {}
  const url = `${pathToFileURL(absPath).href}?fw=${++loadSeq}`
  try {
    return await import(url)
  } catch (err) {
    if (isTypeScriptFile(absPath) && err && (err.code === 'ERR_UNKNOWN_FILE_EXTENSION' || /unknown file extension/i.test(String(err.message)))) {
      throw new Error(
        `Failed to load TypeScript file "${absPath}" on Node.js ${process.versions.node}. Run with --experimental-strip-types (Node 22.6-22.17) or upgrade to Node >= 22.18.`,
        { cause: err },
      )
    }
    throw err
  }
}

async function loadTestFile(filePath, cwd = process.cwd()) {
  const absPath = path.resolve(filePath)
  const relPath = path.relative(cwd, absPath).replace(/\\/g, '/')
  const fileSuite = createSuite('', null, relPath)
  const prevRoot = state.rootSuite
  const prevStack = state.suiteStack
  const prevNextId = state.nextId
  state.rootSuite = fileSuite
  state.suiteStack = [fileSuite]
  state.nextId = 1
  try {
    await importFreshModule(absPath)
    return {
      file: relPath,
      absPath,
      rootSuite: fileSuite,
      tests: flattenSuiteTests(fileSuite, relPath),
    }
  } finally {
    state.rootSuite = prevRoot
    state.suiteStack = prevStack
    state.nextId = prevNextId
  }
}

async function capturePageScreenshot(page, targetPath) {
  if (!page || typeof page.screenshot !== 'function') return null
  try {
    if (typeof page.isClosed === 'function' && page.isClosed()) return null
  } catch {
    return null
  }
  try {
    await fs.mkdir(path.dirname(targetPath), { recursive: true })
    await page.screenshot({ path: targetPath })
    return targetPath
  } catch {
    return null
  }
}

function collectContextVideoHandles(context, extraPage = null) {
  const handles = new Set()
  if (context && typeof context._videoHandles === 'function') {
    try {
      for (const v of context._videoHandles()) {
        if (v) handles.add(v)
      }
    } catch {}
  }
  const pages = new Set()
  if (extraPage) pages.add(extraPage)
  if (context && typeof context.pages === 'function') {
    try {
      for (const p of context.pages()) pages.add(p)
    } catch {}
  }
  for (const p of pages) {
    if (p && typeof p.video === 'function') {
      try {
        const v = p.video()
        if (v) handles.add(v)
      } catch {}
    }
  }
  return [...handles]
}

async function saveOrDiscardVideo(videoHandle, keep, destDir, index, tempDir = null) {
  if (!videoHandle) return null
  if (keep) {
    let srcPath = null
    if (typeof videoHandle.path === 'function') {
      try {
        srcPath = await videoHandle.path()
      } catch {
        srcPath = null
      }
    }
    const ext = (srcPath && path.extname(srcPath)) || '.webm'
    const filename = index === 0 ? `video${ext}` : `video-${index + 1}${ext}`
    const destPath = path.join(destDir, filename)
    await fs.mkdir(destDir, { recursive: true })
    if (typeof videoHandle.saveAs === 'function') {
      try {
        await videoHandle.saveAs(destPath)
        if (typeof videoHandle.delete === 'function') {
          await videoHandle.delete().catch(() => {})
        }
        return {
          name: 'video',
          contentType: ext === '.avi' ? 'video/x-msvideo' : 'video/webm',
          path: destPath,
        }
      } catch {}
    }
    if (srcPath) {
      try {
        await fs.copyFile(srcPath, destPath)
        if (typeof videoHandle.delete === 'function') {
          await videoHandle.delete().catch(() => {})
        }
        return {
          name: 'video',
          contentType: ext === '.avi' ? 'video/x-msvideo' : 'video/webm',
          path: destPath,
        }
      } catch {}
      const resolvedTemp = tempDir ? path.resolve(tempDir) : null
      const resolvedSrc = path.resolve(srcPath)
      const inTemp =
        resolvedTemp &&
        (resolvedSrc === resolvedTemp || resolvedSrc.startsWith(`${resolvedTemp}${path.sep}`))
      if (!inTemp) {
        return {
          name: 'video',
          contentType: ext === '.avi' ? 'video/x-msvideo' : 'video/webm',
          path: srcPath,
        }
      }
    }
    return null
  }
  if (typeof videoHandle.delete === 'function') {
    await videoHandle.delete().catch(() => {})
  }
  return null
}

async function finalizeTestVideos(videoHandles, keepVideo, destDir, videoTempDir) {
  const attachments = []
  const uniqueHandles = [...new Set(videoHandles)]
  let nextIdx = 0
  for (const handle of uniqueHandles) {
    const att = await saveOrDiscardVideo(handle, keepVideo, destDir, nextIdx, videoTempDir)
    if (att) {
      attachments.push(att)
      nextIdx += 1
    }
  }
  if (keepVideo && videoTempDir) {
    try {
      const entries = await fs.readdir(videoTempDir, { withFileTypes: true })
      const leftovers = entries
        .filter((e) => e.isFile() && /\.(?:webm|avi)$/i.test(e.name))
        .map((e) => e.name)
        .sort()
      for (const file of leftovers) {
        const src = path.join(videoTempDir, file)
        const ext = path.extname(file) || '.webm'
        const filename = nextIdx === 0 ? `video${ext}` : `video-${nextIdx + 1}${ext}`
        const destPath = path.join(destDir, filename)
        try {
          await fs.mkdir(destDir, { recursive: true })
          await fs.copyFile(src, destPath)
          attachments.push({
            name: 'video',
            contentType: ext.toLowerCase() === '.avi' ? 'video/x-msvideo' : 'video/webm',
            path: destPath,
          })
          nextIdx += 1
        } catch {}
      }
    } catch {}
  }
  if (videoTempDir) {
    await fs.rm(videoTempDir, { recursive: true, force: true }).catch(() => {})
  }
  return attachments
}

function extensionForContentType(contentType, name) {
  const existingExt = path.extname(String(name || ''))
  if (existingExt && /^\.[a-zA-Z0-9]{1,8}$/.test(existingExt)) return ''
  const mime = String(contentType || '').split(';')[0].trim().toLowerCase()
  if (mime === 'application/json') return '.json'
  if (mime === 'text/plain') return '.txt'
  if (mime === 'text/html') return '.html'
  if (mime === 'text/csv') return '.csv'
  if (mime === 'application/xml' || mime === 'text/xml') return '.xml'
  if (mime === 'application/zip') return '.zip'
  if (mime === 'application/pdf') return '.pdf'
  return '.bin'
}

function resolveMergedUse(ancestors, runOptions = {}) {
  const defaults = {
    screenshot: 'only-on-failure',
    trace: 'off',
    video: 'off',
    headless: true,
    timeout: DEFAULT_TEST_TIMEOUT_MS,
  }
  let merged = { ...defaults }
  if (runOptions.configUse) {
    merged = mergeUseOptions(merged, runOptions.configUse)
  }
  for (const s of ancestors) {
    merged = mergeUseOptions(merged, s.useOptions)
  }
  const explicitCli = runOptions.cliUse !== undefined ? runOptions.cliUse : runOptions
  merged = mergeUseOptions(merged, explicitCli)
  return merged
}

function resolveLaunchOptions(mergedUse) {
  return {
    ...(mergedUse.launchOptions || {}),
    ...(mergedUse.headless !== undefined ? { headless: mergedUse.headless } : {}),
    ...(mergedUse.maxBrowsers !== undefined ? { maxBrowsers: mergedUse.maxBrowsers } : {}),
  }
}

function combineSoftErrors(softErrors) {
  if (softErrors.length === 1) return softErrors[0]
  const details = softErrors
    .map((e, i) => `${i + 1}) ${e instanceof Error ? e.message : String(e)}`)
    .join('\n\n')
  const err = new Error(`${softErrors.length} soft assertion failures:\n\n${details}`)
  err.stack = softErrors
    .map((e, i) => `${i + 1}) ${e instanceof Error ? e.stack || e.message : String(e)}`)
    .join('\n\n')
  return err
}

async function executeSingleAttempt(testCase, pool, runOptions, retryIndex) {
  const startedAt = Date.now()
  const ancestors = collectSuiteAncestors(testCase.suite)
  const mergedUse = resolveMergedUse(ancestors, runOptions)

  const screenshotMode = mergedUse.screenshot || 'only-on-failure'
  const traceMode = mergedUse.trace || 'off'
  const videoMode = mergedUse.video || 'off'
  const testTimeout = mergedUse.timeout ?? DEFAULT_TEST_TIMEOUT_MS
  const outputRoot = path.resolve(runOptions.outputDir || mergedUse.outputDir || 'test-results')
  const retrySuffix = retryIndex > 0 ? `-retry${retryIndex}` : ''
  const testOutputDir = path.join(outputRoot, `${sanitizePathSegment(testCase.fullTitle)}${retrySuffix}`)
  const softErrors = []
  let attachSeq = 0

  const testInfo = {
    title: testCase.title,
    titlePath: testCase.titlePath,
    file: testCase.file,
    retry: retryIndex,
    outputDir: testOutputDir,
    outputPath: (...segments) => path.join(testOutputDir, ...segments),
    status: 'passed',
    error: undefined,
    errors: softErrors,
    attachments: [],
    async attach(name, options = {}) {
      const contentType = options.contentType || 'application/octet-stream'
      const body =
        options.body !== undefined
          ? Buffer.isBuffer(options.body)
            ? options.body
            : Buffer.from(String(options.body))
          : undefined
      let attPath = options.path
      if (body !== undefined && !attPath && !contentType.startsWith('image/')) {
        attachSeq += 1
        const safeName = sanitizePathSegment(name || `attachment-${attachSeq}`)
        const ext = extensionForContentType(contentType, name)
        const base = attachSeq === 1 ? `${safeName}${ext}` : `${safeName}-${attachSeq}${ext}`
        await fs.mkdir(testOutputDir, { recursive: true })
        attPath = path.join(testOutputDir, base)
        await fs.writeFile(attPath, body)
      }
      const att = {
        name,
        contentType,
        path: attPath,
        body,
      }
      testInfo.attachments.push(att)
    },
  }

  if (testCase.skipped) {
    testInfo.status = 'skipped'
    return {
      title: testCase.title,
      titlePath: testCase.titlePath,
      file: testCase.file,
      status: 'skipped',
      durationMs: 0,
      retries: retryIndex,
      error: null,
      attachments: [],
    }
  }

  const beforeHooks = ancestors.flatMap((s) => s.beforeEach)
  const afterHooks = ancestors.slice().reverse().flatMap((s) => s.afterEach)

  const needed = new Set()
  for (const fn of [...beforeHooks, testCase.fn, ...afterHooks]) {
    for (const k of requestedFixtures(fn)) needed.add(k)
  }

  const needPage = needed.has('page')
  const needContext = needPage || needed.has('context')
  const needBrowser = needContext || needed.has('browser')

  const launchOpts = resolveLaunchOptions(mergedUse)

  let browser = null
  let context = null
  let page = null
  let tracing = null
  let tracingStarted = false
  let videoTempDir = null
  let videoHandles = []
  let skippedDynamically = false
  let caughtError = null

  const prevActiveInfo = state.activeTestInfo
  const prevActiveSoftErrors = state.activeSoftErrors
  state.activeTestInfo = testInfo
  state.activeSoftErrors = softErrors
  const prevSoftCollector = setSoftErrorCollector((err) => {
    softErrors.push(err instanceof Error ? err : new Error(String(err)))
  })

  try {
    if (needBrowser) {
      browser = await pool.getBrowser(launchOpts)
    }

    if (needContext) {
      const ctxOpts = pickKeys(mergedUse, CONTEXT_OPTION_KEYS)
      let videoConfig = null
      if (videoMode === 'on' || videoMode === 'retain-on-failure') {
        videoTempDir = path.join(testOutputDir, '.video-tmp')
        await fs.mkdir(videoTempDir, { recursive: true })
        videoConfig = {
          dir: videoTempDir,
          ...(ctxOpts.recordVideo?.size ? { size: ctxOpts.recordVideo.size } : {}),
        }
      }
      context = await openContextWithFeatureDetection(browser, ctxOpts, videoConfig)

      if (
        context &&
        context.tracing &&
        typeof context.tracing.start === 'function' &&
        typeof context.tracing.stop === 'function' &&
        (traceMode === 'on' || traceMode === 'retain-on-failure')
      ) {
        tracing = context.tracing
        await tracing.start({ screenshots: true, snapshots: true })
        tracingStarted = true
      }

      if (needPage) {
        page = await context.newPage()
        if (mergedUse.actionTimeout !== undefined && typeof page.setDefaultTimeout === 'function') {
          page.setDefaultTimeout(mergedUse.actionTimeout)
        }
        if (mergedUse.navigationTimeout !== undefined && typeof page.setDefaultNavigationTimeout === 'function') {
          page.setDefaultNavigationTimeout(mergedUse.navigationTimeout)
        }
      }
    }

    const fixtures = {
      browser,
      context,
      page,
      baseURL: mergedUse.baseURL,
    }

    await runWithTimeout(async () => {
      for (const hook of beforeHooks) {
        await hook(fixtures, testInfo)
      }
      await testCase.fn(fixtures, testInfo)
    }, testTimeout, testCase.title)

    if (!skippedDynamically && softErrors.length > 0) {
      caughtError = combineSoftErrors(softErrors)
      testInfo.status = 'failed'
      testInfo.error = caughtError
    }
  } catch (err) {
    if (err instanceof SkipSignal) {
      skippedDynamically = true
      testInfo.status = 'skipped'
    } else {
      caughtError = err instanceof Error ? err : new Error(String(err))
      testInfo.status = caughtError.name === 'TimeoutError' ? 'timedOut' : 'failed'
      testInfo.error = caughtError
    }
  } finally {
    const failedBeforeAfterHooks = Boolean(caughtError)
    if (!skippedDynamically) {
      const captureOnTestOutcome =
        screenshotMode === 'on' || (screenshotMode === 'only-on-failure' && failedBeforeAfterHooks)
      if (captureOnTestOutcome && context) {
        const candidates = []
        if (page) candidates.push(page)
        if (typeof context.pages === 'function') {
          for (const p of context.pages()) {
            if (!candidates.includes(p)) candidates.push(p)
          }
        }
        let shotIdx = 1
        for (const pg of candidates) {
          const shotName = failedBeforeAfterHooks ? `test-failed-${shotIdx}.png` : `test-finished-${shotIdx}.png`
          const shotPath = path.join(testOutputDir, shotName)
          const saved = await capturePageScreenshot(pg, shotPath)
          if (saved) {
            testInfo.attachments.push({ name: 'screenshot', contentType: 'image/png', path: saved })
            shotIdx += 1
          }
        }
      }
    }

    // Run afterEach hooks even on failure.
    const fixturesForAfter = {
      browser,
      context,
      page,
      baseURL: mergedUse.baseURL,
    }
    for (const hook of afterHooks) {
      try {
        await hook(fixturesForAfter, testInfo)
      } catch (hookErr) {
        if (!caughtError && !(hookErr instanceof SkipSignal)) {
          caughtError = hookErr instanceof Error ? hookErr : new Error(String(hookErr))
          testInfo.status = 'failed'
          testInfo.error = caughtError
        }
      }
    }
    if (!caughtError && !skippedDynamically && softErrors.length > 0) {
      caughtError = combineSoftErrors(softErrors)
      testInfo.status = 'failed'
      testInfo.error = caughtError
    }

    const finalFailed = Boolean(caughtError)

    // Collect video handles only when runner owns video recording (videoMode !== 'off').
    if (context && videoTempDir) {
      videoHandles = collectContextVideoHandles(context, page)
    }

    if (tracing && tracingStarted) {
      const keepTrace = traceMode === 'on' || (traceMode === 'retain-on-failure' && finalFailed)
      try {
        if (keepTrace) {
          await fs.mkdir(testOutputDir, { recursive: true })
          const tracePath = path.join(testOutputDir, 'trace.zip')
          await tracing.stop({ path: tracePath })
          testInfo.attachments.push({ name: 'trace', contentType: 'application/zip', path: tracePath })
        } else {
          await tracing.stop()
        }
      } catch {}
    }

    if (context) {
      await context.close().catch(() => {})
    }

    if (videoTempDir) {
      const keepVideo = videoMode === 'on' || (videoMode === 'retain-on-failure' && finalFailed)
      const videoAtts = await finalizeTestVideos(videoHandles, keepVideo, testOutputDir, videoTempDir)
      testInfo.attachments.push(...videoAtts)
    }

    setSoftErrorCollector(prevSoftCollector)
    state.activeSoftErrors = prevActiveSoftErrors
    state.activeTestInfo = prevActiveInfo
  }

  return {
    title: testCase.title,
    titlePath: testCase.titlePath,
    file: testCase.file,
    status: testInfo.status,
    durationMs: Date.now() - startedAt,
    retries: retryIndex,
    error: caughtError ? { message: caughtError.message, stack: caughtError.stack } : null,
    attachments: testInfo.attachments.map((att) => ({
      name: att.name,
      contentType: att.contentType,
      path: att.path,
      dataUri: att.body && att.contentType?.startsWith('image/')
        ? `data:${att.contentType};base64,${att.body.toString('base64')}`
        : undefined,
    })),
  }
}

async function runTestsForFile(fileEntry, selectedIds, pool, runOptions = {}, onResult = null) {
  const selectedSet = selectedIds ? new Set(selectedIds) : null
  const testsToRun = fileEntry.tests.filter((t) => !selectedSet || selectedSet.has(t.id))
  const retries = Math.max(0, Number(runOptions.retries || 0))
  const results = []

  const enteredSuites = []
  const enteredSet = new Set()
  const startedBeforeAll = new Set()
  const failedBeforeAll = new Map()

  const markEntered = (suite) => {
    if (!enteredSet.has(suite)) {
      enteredSet.add(suite)
      enteredSuites.push(suite)
    }
  }

  const suiteTitlePath = (suite, hookName) => {
    const ancestors = collectSuiteAncestors(suite)
    const parts = []
    if (fileEntry.file) parts.push(fileEntry.file)
    for (const a of ancestors) {
      if (a.title) parts.push(a.title)
    }
    parts.push(hookName)
    return parts
  }

  try {
    for (const testCase of testsToRun) {
      const ancestors = collectSuiteAncestors(testCase.suite)
      let suiteSetupErr = null
      if (!testCase.skipped) {
        for (const s of ancestors) {
          if (failedBeforeAll.has(s)) {
            suiteSetupErr = failedBeforeAll.get(s)
            break
          }
          markEntered(s)
          if (s.beforeAll.length && !startedBeforeAll.has(s)) {
            startedBeforeAll.add(s)
            try {
              const needsBrowser = s.beforeAll.some((fn) => requestedFixtures(fn).has('browser'))
              const suiteLaunchOpts = resolveLaunchOptions(resolveMergedUse(collectSuiteAncestors(s), runOptions))
              const browser = needsBrowser ? await pool.getBrowser(suiteLaunchOpts) : null
              for (const hook of s.beforeAll) {
                await hook({ browser })
              }
            } catch (err) {
              const wrapped = err instanceof Error ? err : new Error(String(err))
              failedBeforeAll.set(s, wrapped)
              suiteSetupErr = wrapped
              break
            }
          }
        }
      }

      if (suiteSetupErr) {
        const failRes = {
          title: testCase.title,
          titlePath: testCase.titlePath,
          file: testCase.file,
          status: 'failed',
          durationMs: 0,
          retries: 0,
          error: { message: suiteSetupErr.message, stack: suiteSetupErr.stack },
          attachments: [],
        }
        results.push(failRes)
        if (typeof onResult === 'function') await onResult(failRes)
        continue
      }

      let finalResult = null
      let lastFailureError = null
      const priorFailureAttachments = []
      const maxAttempts = testCase.skipped ? 1 : retries + 1
      for (let attempt = 0; attempt < maxAttempts; attempt++) {
        const res = await executeSingleAttempt(testCase, pool, runOptions, attempt)
        finalResult = res
        if (res.status === 'passed' || res.status === 'skipped') {
          if (attempt > 0 && res.status === 'passed') {
            finalResult = {
              ...res,
              status: 'flaky',
              retries: attempt,
              error: res.error ?? lastFailureError,
              attachments: [...priorFailureAttachments, ...(res.attachments || [])],
            }
          }
          break
        }
        if (res.error) lastFailureError = res.error
        if (Array.isArray(res.attachments) && res.attachments.length > 0) {
          priorFailureAttachments.push(...res.attachments)
        }
      }
      results.push(finalResult)
      if (typeof onResult === 'function') {
        await onResult(finalResult)
      }
    }
  } finally {
    for (const s of enteredSuites.slice().reverse()) {
      if (!s.afterAll.length) continue
      const suiteLaunchOpts = resolveLaunchOptions(resolveMergedUse(collectSuiteAncestors(s), runOptions))
      const needsBrowser = s.afterAll.some((fn) => requestedFixtures(fn).has('browser'))
      let browser = null
      let browserErr = null
      if (needsBrowser) {
        try {
          browser = await pool.getBrowser(suiteLaunchOpts)
        } catch (err) {
          browserErr = err instanceof Error ? err : new Error(String(err))
        }
      }
      if (browserErr) {
        const failRes = {
          title: 'afterAll',
          titlePath: suiteTitlePath(s, 'afterAll'),
          file: fileEntry.file,
          status: 'failed',
          durationMs: 0,
          retries: 0,
          error: { message: browserErr.message, stack: browserErr.stack },
          attachments: [],
        }
        results.push(failRes)
        if (typeof onResult === 'function') await onResult(failRes)
        continue
      }
      for (const hook of s.afterAll) {
        const hookStart = Date.now()
        try {
          await hook({ browser })
        } catch (err) {
          const wrapped = err instanceof Error ? err : new Error(String(err))
          const failRes = {
            title: 'afterAll',
            titlePath: suiteTitlePath(s, 'afterAll'),
            file: fileEntry.file,
            status: 'failed',
            durationMs: Date.now() - hookStart,
            retries: 0,
            error: { message: wrapped.message, stack: wrapped.stack },
            attachments: [],
          }
          results.push(failRes)
          if (typeof onResult === 'function') await onResult(failRes)
        }
      }
    }
  }

  return results
}

function createFluxwrightFixtures(options = {}) {
  const launchOpts = {
    ...(options.launchOptions || {}),
    ...(options.headless !== undefined ? { headless: options.headless } : {}),
    ...(options.maxBrowsers !== undefined ? { maxBrowsers: options.maxBrowsers } : {}),
  }
  const baseContextOpts = pickKeys(
    {
      ...(options.contextOptions || {}),
      ...(options.baseURL !== undefined ? { baseURL: options.baseURL } : {}),
      ...(options.viewport !== undefined ? { viewport: options.viewport } : {}),
    },
    CONTEXT_OPTION_KEYS,
  )
  const screenshotMode = options.screenshot ?? 'only-on-failure'
  const traceMode = options.trace ?? 'off'
  const videoMode = options.video ?? 'off'
  const outputDir = path.resolve(options.outputDir || 'test-results')
  const pool = createBrowserPool(launchOpts)

  async function getBrowser() {
    return pool.getBrowser()
  }

  async function close() {
    await pool.closeAll()
  }

  async function acquire(testContextOrOpts) {
    const isNodeTestCtx = testContextOrOpts && typeof testContextOrOpts.after === 'function'
    const testName = isNodeTestCtx ? (testContextOrOpts.name || 'node-test') : (testContextOrOpts?.name || 'fixture-test')
    const extraCtxOpts = isNodeTestCtx ? {} : pickKeys(testContextOrOpts || {}, CONTEXT_OPTION_KEYS)
    const testOutputDir = path.join(outputDir, sanitizePathSegment(testName))
    const mergedCtxOpts = { ...baseContextOpts, ...extraCtxOpts }
    const browser = await getBrowser()

    let videoTempDir = null
    let videoConfig = null
    if (videoMode === 'on' || videoMode === 'retain-on-failure') {
      videoTempDir = path.join(testOutputDir, '.video-tmp')
      await fs.mkdir(videoTempDir, { recursive: true })
      videoConfig = {
        dir: videoTempDir,
        ...(mergedCtxOpts.recordVideo?.size ? { size: mergedCtxOpts.recordVideo.size } : {}),
      }
    }

    const context = await openContextWithFeatureDetection(browser, mergedCtxOpts, videoConfig)
    let tracingStarted = false
    if (
      context?.tracing &&
      typeof context.tracing.start === 'function' &&
      typeof context.tracing.stop === 'function' &&
      (traceMode === 'on' || traceMode === 'retain-on-failure')
    ) {
      await context.tracing.start({ screenshots: true, snapshots: true })
      tracingStarted = true
    }

    const page = await context.newPage()

    let disposed = false
    async function dispose(failed = false) {
      if (disposed) return
      disposed = true
      const videoHandles = videoTempDir ? collectContextVideoHandles(context, page) : []
      try {
        const shouldShot = screenshotMode === 'on' || (screenshotMode === 'only-on-failure' && failed)
        if (shouldShot && !page.isClosed()) {
          const shotPath = path.join(testOutputDir, failed ? 'test-failed-1.png' : 'test-finished-1.png')
          await capturePageScreenshot(page, shotPath)
        }
        if (tracingStarted) {
          const keepTrace = traceMode === 'on' || (traceMode === 'retain-on-failure' && failed)
          try {
            if (keepTrace) {
              await fs.mkdir(testOutputDir, { recursive: true })
              await context.tracing.stop({ path: path.join(testOutputDir, 'trace.zip') })
            } else {
              await context.tracing.stop()
            }
          } catch {}
        }
      } finally {
        await context.close().catch(() => {})
        if (videoTempDir) {
          const keepVideo = videoMode === 'on' || (videoMode === 'retain-on-failure' && failed)
          await finalizeTestVideos(videoHandles, keepVideo, testOutputDir, videoTempDir)
        }
      }
    }

    if (isNodeTestCtx) {
      testContextOrOpts.after(() => dispose(false))
    }

    return { browser, context, page, dispose }
  }

  async function withPage(fn, testContextOrOpts) {
    const { browser, context, page, dispose } = await acquire(
      testContextOrOpts && typeof testContextOrOpts.after === 'function'
        ? { name: testContextOrOpts.name }
        : testContextOrOpts,
    )
    let failed = false
    try {
      return await fn({ browser, context, page })
    } catch (err) {
      failed = true
      throw err
    } finally {
      await dispose(failed)
    }
  }

  let vitestSeq = 0
  const vitest = {
    browser: async ({}, use) => {
      const browser = await getBrowser()
      await use(browser)
    },
    context: async ({ browser }, use, task) => {
      const testName = task?.name || `vitest-${++vitestSeq}`
      const testOutputDir = path.join(outputDir, sanitizePathSegment(testName))
      let videoTempDir = null
      let videoConfig = null
      if (videoMode === 'on' || videoMode === 'retain-on-failure') {
        videoTempDir = path.join(testOutputDir, '.video-tmp')
        await fs.mkdir(videoTempDir, { recursive: true })
        videoConfig = {
          dir: videoTempDir,
          ...(baseContextOpts.recordVideo?.size ? { size: baseContextOpts.recordVideo.size } : {}),
        }
      }
      const context = await openContextWithFeatureDetection(browser, baseContextOpts, videoConfig)
      let tracingStarted = false
      if (
        context?.tracing &&
        typeof context.tracing.start === 'function' &&
        typeof context.tracing.stop === 'function' &&
        (traceMode === 'on' || traceMode === 'retain-on-failure')
      ) {
        await context.tracing.start({ screenshots: true, snapshots: true })
        tracingStarted = true
      }
      let failed = false
      try {
        await use(context)
        if (task?.result?.state === 'fail') failed = true
      } catch (err) {
        failed = true
        throw err
      } finally {
        const videoHandles = videoTempDir ? collectContextVideoHandles(context) : []
        try {
          const shouldShot = screenshotMode === 'on' || (screenshotMode === 'only-on-failure' && failed)
          if (shouldShot && typeof context.pages === 'function') {
            let shotIdx = 1
            for (const pg of context.pages()) {
              const shotPath = path.join(testOutputDir, failed ? `test-failed-${shotIdx}.png` : `test-finished-${shotIdx}.png`)
              const saved = await capturePageScreenshot(pg, shotPath)
              if (saved) shotIdx += 1
            }
          }
          if (tracingStarted) {
            const keepTrace = traceMode === 'on' || (traceMode === 'retain-on-failure' && failed)
            try {
              if (keepTrace) {
                await fs.mkdir(testOutputDir, { recursive: true })
                await context.tracing.stop({ path: path.join(testOutputDir, 'trace.zip') })
              } else {
                await context.tracing.stop()
              }
            } catch {}
          }
        } finally {
          await context.close().catch(() => {})
          if (videoTempDir) {
            const keepVideo = videoMode === 'on' || (videoMode === 'retain-on-failure' && failed)
            await finalizeTestVideos(videoHandles, keepVideo, testOutputDir, videoTempDir)
          }
        }
      }
    },
    page: async ({ context }, use) => {
      const page = await context.newPage()
      await use(page)
    },
  }

  return {
    getBrowser,
    setup: getBrowser,
    close,
    teardown: close,
    acquire,
    withPage,
    vitest,
    expect,
    getLaunchCount: pool.getLaunchCount,
  }
}

const _internals = {
  createBrowserPool,
  importFreshModule,
  isTypeScriptFile,
  loadTestFile,
  needsStripTypesFlag,
  requestedFixtures,
  runTestsForFile,
  sanitizePathSegment,
  supportsNativeTypeScript,
}

module.exports = {
  test,
  expect,
  defineConfig,
  createFluxwrightFixtures,
  _internals,
}
