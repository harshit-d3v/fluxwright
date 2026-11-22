'use strict'

const assert = require('node:assert/strict')
const crypto = require('node:crypto')
const fs = require('node:fs/promises')
const path = require('node:path')
const { pathToFileURL } = require('node:url')
const { isDeepStrictEqual } = require('node:util')
const fluxwright = require('./addon.js')

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

  constructor(actual, negated = false) {
    this.#actual = actual
    this.#negated = negated
  }

  get not() {
    return new ValueAssertions(this.#actual, !this.#negated)
  }

  #assert(pass, matcher, expectedDesc, actualDesc = formatVal(this.#actual)) {
    const ok = this.#negated ? !pass : pass
    if (ok) return
    const prefix = this.#negated ? 'not ' : ''
    throw new assert.AssertionError({
      message: `expect(received).${this.#negated ? 'not.' : ''}${matcher}\nExpected: ${prefix}${expectedDesc}\nReceived: ${actualDesc}`,
      actual: this.#actual,
      expected: expectedDesc,
      operator: matcher,
    })
  }

  toBe(expected) {
    this.#assert(Object.is(this.#actual, expected), 'toBe', formatVal(expected))
  }

  toEqual(expected) {
    let eq = isDeepStrictEqual(this.#actual, expected)
    if (!eq && typeof this.#actual === 'object' && this.#actual !== null && typeof expected === 'object' && expected !== null) {
      try {
        assert.deepEqual(this.#actual, expected)
        eq = true
      } catch {
        eq = false
      }
    }
    this.#assert(eq, 'toEqual', formatVal(expected))
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
    const re = pattern instanceof RegExp ? new RegExp(pattern.source, pattern.flags.replace(/[gy]/g, '')) : null
    const ok = re ? re.test(String(this.#actual)) : String(this.#actual).includes(String(pattern))
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
        pass = expected.test(msg)
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
    return new ValueAssertions(target, false)
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
  const out = { ...base, ...extra }
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
  return src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\n]*/g, '')
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
      if (firstParam[i] === '{') braceDepth += 1
      else if (firstParam[i] === '}') {
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

function sanitizePathSegment(name) {
  const cleaned = String(name || 'test')
    .replace(/[^a-zA-Z0-9._-]+/g, '-')
    .replace(/-+/g, '-')
    .replace(/^-|-$/g, '')
  if (!cleaned) return 'test'
  if (cleaned.length <= 100) return cleaned
  const hash = crypto.createHash('sha1').update(String(name)).digest('hex').slice(0, 8)
  return `${cleaned.slice(0, 91)}-${hash}`
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
    const url = pathToFileURL(absPath).href
    await import(url)
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

async function saveOrDiscardVideo(videoHandle, keep, destDir, index) {
  if (!videoHandle) return null
  if (keep) {
    let ext = '.webm'
    if (typeof videoHandle.path === 'function') {
      try {
        const srcPath = await videoHandle.path()
        const parsedExt = srcPath ? path.extname(srcPath) : ''
        if (parsedExt) ext = parsedExt
      } catch {}
    }
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
    if (typeof videoHandle.path === 'function') {
      try {
        const srcPath = await videoHandle.path()
        if (srcPath) {
          return {
            name: 'video',
            contentType: ext === '.avi' ? 'video/x-msvideo' : 'video/webm',
            path: srcPath,
          }
        }
      } catch {}
    }
    return null
  }
  if (typeof videoHandle.delete === 'function') {
    await videoHandle.delete().catch(() => {})
  }
  return null
}

async function executeSingleAttempt(testCase, pool, runOptions, retryIndex) {
  const startedAt = Date.now()
  const ancestors = collectSuiteAncestors(testCase.suite)
  let mergedUse = {}
  for (const s of ancestors) {
    mergedUse = mergeUseOptions(mergedUse, s.useOptions)
  }
  mergedUse = mergeUseOptions(mergedUse, runOptions)

  const screenshotMode = mergedUse.screenshot || 'only-on-failure'
  const traceMode = mergedUse.trace || 'off'
  const videoMode = mergedUse.video || 'off'
  const testTimeout = mergedUse.timeout ?? DEFAULT_TEST_TIMEOUT_MS
  const outputRoot = path.resolve(mergedUse.outputDir || 'test-results')
  const retrySuffix = retryIndex > 0 ? `-retry${retryIndex}` : ''
  const testOutputDir = path.join(outputRoot, `${sanitizePathSegment(testCase.fullTitle)}${retrySuffix}`)

  const testInfo = {
    title: testCase.title,
    titlePath: testCase.titlePath,
    file: testCase.file,
    retry: retryIndex,
    outputDir: testOutputDir,
    outputPath: (...segments) => path.join(testOutputDir, ...segments),
    status: 'passed',
    error: undefined,
    attachments: [],
    async attach(name, options = {}) {
      const att = {
        name,
        contentType: options.contentType || 'application/octet-stream',
        path: options.path,
        body: options.body !== undefined
          ? (Buffer.isBuffer(options.body) ? options.body : Buffer.from(String(options.body)))
          : undefined,
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

  const launchOpts = {
    ...(mergedUse.launchOptions || {}),
    ...(mergedUse.headless !== undefined ? { headless: mergedUse.headless } : {}),
    ...(mergedUse.maxBrowsers !== undefined ? { maxBrowsers: mergedUse.maxBrowsers } : {}),
  }

  let browser = null
  let context = null
  let page = null
  let tracing = null
  let tracingStarted = false
  let videoTempDir = null
  const videoHandles = []
  let skippedDynamically = false
  let caughtError = null

  const prevActiveInfo = state.activeTestInfo
  state.activeTestInfo = testInfo

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
        videoConfig = { dir: videoTempDir }
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

    // Run afterEach hooks even on failure (unless dynamically skipped in beforeEach? run anyway safely).
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

    const finalFailed = Boolean(caughtError)

    // Collect video handles before closing context.
    if (context) {
      const seenPages = new Set()
      if (page) seenPages.add(page)
      if (typeof context.pages === 'function') {
        for (const p of context.pages()) seenPages.add(p)
      }
      for (const p of seenPages) {
        if (p && typeof p.video === 'function') {
          try {
            const v = p.video()
            if (v) videoHandles.push(v)
          } catch {}
        }
      }
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

    const keepVideo = videoMode === 'on' || (videoMode === 'retain-on-failure' && finalFailed)
    for (let i = 0; i < videoHandles.length; i++) {
      const videoAtt = await saveOrDiscardVideo(videoHandles[i], keepVideo, testOutputDir, i)
      if (videoAtt) testInfo.attachments.push(videoAtt)
    }
    if (videoTempDir) {
      await fs.rm(videoTempDir, { recursive: true, force: true }).catch(() => {})
    }

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

  // Determine suites that have at least one non-skipped test to run beforeAll/afterAll hooks.
  const activeSuites = new Set()
  for (const t of testsToRun) {
    if (!t.skipped) {
      for (const s of collectSuiteAncestors(t.suite)) activeSuites.add(s)
    }
  }

  const startedBeforeAll = new Set()
  const failedBeforeAll = new Map()
  const orderedSuitesWithAll = [...activeSuites]

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
          if (s.beforeAll.length && !startedBeforeAll.has(s)) {
            startedBeforeAll.add(s)
            try {
              const needsBrowser = s.beforeAll.some((fn) => requestedFixtures(fn).has('browser'))
              const browser = needsBrowser ? await pool.getBrowser(runOptions) : null
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
      const maxAttempts = testCase.skipped ? 1 : retries + 1
      for (let attempt = 0; attempt < maxAttempts; attempt++) {
        const res = await executeSingleAttempt(testCase, pool, runOptions, attempt)
        finalResult = res
        if (res.status === 'passed' || res.status === 'skipped') {
          if (attempt > 0 && res.status === 'passed') {
            finalResult = { ...res, status: 'flaky', retries: attempt }
          }
          break
        }
      }
      results.push(finalResult)
      if (typeof onResult === 'function') {
        await onResult(finalResult)
      }
    }
  } finally {
    for (const s of orderedSuitesWithAll.reverse()) {
      if (s.afterAll.length && startedBeforeAll.has(s)) {
        const needsBrowser = s.afterAll.some((fn) => requestedFixtures(fn).has('browser'))
        const browser = needsBrowser ? await pool.getBrowser(runOptions).catch(() => null) : null
        for (const hook of s.afterAll) {
          await hook({ browser }).catch(() => {})
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
    { ...(options.contextOptions || {}), ...(options.baseURL !== undefined ? { baseURL: options.baseURL } : {}), ...(options.viewport !== undefined ? { viewport: options.viewport } : {}) },
    CONTEXT_OPTION_KEYS,
  )
  const screenshotMode = options.screenshot ?? 'only-on-failure'
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
    const browser = await getBrowser()
    const context = await browser.newContext({ ...baseContextOpts, ...extraCtxOpts })
    const page = await context.newPage()

    let disposed = false
    async function dispose(failed = false) {
      if (disposed) return
      disposed = true
      try {
        const shouldShot = screenshotMode === 'on' || (screenshotMode === 'only-on-failure' && failed)
        if (shouldShot && !page.isClosed()) {
          const shotPath = path.join(outputDir, sanitizePathSegment(testName), failed ? 'test-failed-1.png' : 'test-finished-1.png')
          await capturePageScreenshot(page, shotPath)
        }
      } finally {
        await context.close().catch(() => {})
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

  const vitest = {
    browser: async ({}, use) => {
      const browser = await getBrowser()
      await use(browser)
    },
    context: async ({ browser }, use) => {
      const context = await browser.newContext(baseContextOpts)
      try {
        await use(context)
      } finally {
        await context.close().catch(() => {})
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
  loadTestFile,
  requestedFixtures,
  runTestsForFile,
  sanitizePathSegment,
}

module.exports = {
  test,
  expect,
  defineConfig,
  createFluxwrightFixtures,
  _internals,
}
