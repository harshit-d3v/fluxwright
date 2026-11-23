'use strict'

const fs = require('node:fs/promises')
const { dirname } = require('node:path')
const {
  decodePng,
  compareImages,
  isWithinTolerance,
  artifactPaths,
  shouldUpdateSnapshots,
  validateMaskColor,
  resolveMaskSelectors,
  DISABLE_ANIMATIONS_PAGE_FN,
  RESTORE_ANIMATIONS_PAGE_FN,
  DISABLE_ANIMATIONS_LOCATOR_FN,
  RESTORE_ANIMATIONS_LOCATOR_FN,
} = require('./visual.js')

const DEFAULT_TIMEOUT_MS = 5000
const POLL_INTERVAL_MS = 50
const MIN_PROBE_TIMEOUT_MS = 50
const MAX_PROBE_TIMEOUT_MS = 500

function normalizeWhitespace(s) {
  return String(s).replace(/\s+/g, ' ').trim()
}

function compileRegExp(re) {
  const copy = new RegExp(re.source, re.flags.replace(/g/g, ''))
  copy.lastIndex = 0
  return copy
}

function validatePattern(expected, method) {
  if (typeof expected !== 'string' && !(expected instanceof RegExp)) {
    throw new TypeError(`${method}: expected a string or RegExp`)
  }
}

function validateOptions(method, options, extraAllowed = {}, includeTimeout = true) {
  if (options == null) return {}
  if (typeof options !== 'object' || Array.isArray(options)) {
    throw new TypeError(`${method}: options must be an object`)
  }
  const allowed = includeTimeout ? { timeout: true, ...extraAllowed } : { ...extraAllowed }
  for (const [key, value] of Object.entries(options)) {
    if (value === undefined) continue
    if (!Object.hasOwn(allowed, key)) {
      throw new TypeError(`${method}: option '${key}' is not supported`)
    }
    if (includeTimeout && key === 'timeout' && value != null) {
      if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) {
        throw new TypeError(`${method}: timeout must be a non-negative number`)
      }
    }
    if (key === 'checked' && value != null && typeof value !== 'boolean') {
      throw new TypeError(`${method}: checked must be a boolean`)
    }
  }
  return options
}

function matchPattern(actual, expected, { normalize = false, substring = false } = {}) {
  const actualVal = normalize ? normalizeWhitespace(actual) : actual
  if (typeof expected === 'string') {
    const expectedVal = normalize ? normalizeWhitespace(expected) : expected
    return substring ? actualVal.includes(expectedVal) : actualVal === expectedVal
  }
  return compileRegExp(expected).test(actualVal)
}

function formatExpected(expected, normalize = false) {
  if (expected instanceof RegExp) return String(expected)
  return JSON.stringify(normalize ? normalizeWhitespace(expected) : expected)
}

function formatLocator(locator) {
  const frames = typeof locator._frames === 'function' ? locator._frames() : []
  const query = typeof locator._query === 'function' ? locator._query() : ''
  const parts = [...frames, query].filter(Boolean)
  return parts.length ? `expect(locator(${JSON.stringify(parts.join(' >> '))}))` : 'expect(locator)'
}

function probeTimeout(remainingMs) {
  if (!Number.isFinite(remainingMs)) return MAX_PROBE_TIMEOUT_MS
  return Math.max(MIN_PROBE_TIMEOUT_MS, Math.min(remainingMs, MAX_PROBE_TIMEOUT_MS))
}

function isFatalError(err) {
  const msg = String(err?.message ?? err ?? '')
  return (
    /page (is )?closed/i.test(msg) ||
    /browser process exited/i.test(msg) ||
    /session .* is gone/i.test(msg) ||
    /target crashed/i.test(msg) ||
    /invalid selector:/i.test(msg) ||
    /devtools websocket:/i.test(msg) ||
    /engine is shut down/i.test(msg) ||
    /browser context is closed/i.test(msg) ||
    /target closed/i.test(msg) ||
    /session with given id not found/i.test(msg)
  )
}

function resolveUrl(page, expected) {
  if (typeof expected !== 'string') return expected
  const baseUrl = typeof page._baseUrl === 'function' ? page._baseUrl() : null
  if (baseUrl != null && !/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(expected)) {
    return new URL(expected, baseUrl).href
  }
  return expected
}

async function raceTimeout(promise, limitMs, label) {
  const p = Promise.resolve(promise)
  if (!Number.isFinite(limitMs) || limitMs <= 0) return p
  p.catch(() => {})
  let timer = null
  try {
    return await Promise.race([
      p,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`${label}: timed out after ${limitMs}ms`)), limitMs)
      }),
    ])
  } finally {
    if (timer) clearTimeout(timer)
  }
}

async function pollUntil({ targetLabel, matcher, expectedDesc, negated, timeoutMs, probe }) {
  const unlimited = timeoutMs === 0
  const start = Date.now()
  while (true) {
    const elapsed = Date.now() - start
    const remaining = unlimited ? Infinity : Math.max(0, timeoutMs - elapsed)
    let outcome
    try {
      outcome = await probe(probeTimeout(remaining))
    } catch (err) {
      if (isFatalError(err)) throw err
      outcome = { kind: 'transient', message: String(err?.message ?? err) }
    }

    if (outcome.kind === 'value') {
      const ok = negated ? !outcome.matched : outcome.matched
      if (ok) return
    }

    const actualDesc =
      outcome.kind === 'value'
        ? outcome.actual
        : outcome.kind === 'not-attached'
          ? 'element not found (count: 0)'
          : `error: ${outcome.message}`

    const totalElapsed = Date.now() - start
    if (!unlimited && totalElapsed >= timeoutMs) {
      const expectedText = negated ? `not ${expectedDesc}` : expectedDesc
      throw new Error(
        `${targetLabel}.${matcher}: Timed out ${timeoutMs}ms waiting for assertion.\n` +
          `Expected: ${expectedText}\n` +
          `Received: ${actualDesc}`,
      )
    }

    const waitMs = unlimited
      ? POLL_INTERVAL_MS
      : Math.min(POLL_INTERVAL_MS, Math.max(0, timeoutMs - totalElapsed))
    if (waitMs > 0) {
      await new Promise((resolve) => setTimeout(resolve, waitMs))
    }
  }
}

class LocatorAssertions {
  #locator
  #defaultTimeout
  #negated
  #getOwnerPage
  #onSoftError

  constructor(locator, defaultTimeout, negated = false, getOwnerPage = null, onSoftError = null) {
    this.#locator = locator
    this.#defaultTimeout = defaultTimeout
    this.#negated = negated
    this.#getOwnerPage = getOwnerPage
    this.#onSoftError = onSoftError
  }

  get not() {
    return new LocatorAssertions(this.#locator, this.#defaultTimeout, !this.#negated, this.#getOwnerPage, this.#onSoftError)
  }

  #timeout(options) {
    return options?.timeout ?? this.#defaultTimeout
  }

  async #run(fn) {
    try {
      return await fn()
    } catch (err) {
      if (this.#onSoftError && !(err instanceof TypeError)) {
        this.#onSoftError(err)
        return
      }
      throw err
    }
  }

  #countWithTimeout(limitMs) {
    const target =
      typeof this.#locator._withTimeout === 'function' && Number.isFinite(limitMs) && limitMs > 0
        ? this.#locator._withTimeout(limitMs)
        : this.#locator
    return raceTimeout(target.count(), limitMs, 'locator.count')
  }

  async #ensureAttached(limitMs) {
    try {
      const n = await this.#countWithTimeout(limitMs)
      if (n === 0) return { kind: 'not-attached' }
      return null
    } catch (err) {
      if (isFatalError(err)) throw err
      return { kind: 'transient', message: String(err?.message ?? err) }
    }
  }

  async #boolCheck({ matcher, expected, trueLabel, falseLabel, requireAttached, options, fn }) {
    const opts = validateOptions(`expect(locator).${matcher}`, options)
    const timeoutMs = this.#timeout(opts)
    return this.#run(() =>
      pollUntil({
        targetLabel: formatLocator(this.#locator),
        matcher,
        expectedDesc: expected,
        negated: this.#negated,
        timeoutMs,
        probe: async (limitMs) => {
          if (requireAttached) {
            const early = await this.#ensureAttached(limitMs)
            if (early) return early
          }
          const val = await fn(limitMs)
          return { kind: 'value', matched: val, actual: val ? trueLabel : falseLabel }
        },
      }),
    )
  }

  async toBeVisible(options) {
    return this.#boolCheck({
      matcher: 'toBeVisible',
      expected: 'visible',
      trueLabel: 'visible',
      falseLabel: 'hidden',
      requireAttached: false,
      options,
      fn: (limitMs) => this.#locator.isVisible({ timeout: limitMs }),
    })
  }

  async toBeHidden(options) {
    return this.#boolCheck({
      matcher: 'toBeHidden',
      expected: 'hidden',
      trueLabel: 'hidden',
      falseLabel: 'visible',
      requireAttached: false,
      options,
      fn: (limitMs) => this.#locator.isHidden({ timeout: limitMs }),
    })
  }

  async toBeAttached(options) {
    const opts = validateOptions('expect(locator).toBeAttached', options)
    return this.#run(() =>
      pollUntil({
        targetLabel: formatLocator(this.#locator),
        matcher: 'toBeAttached',
        expectedDesc: 'attached',
        negated: this.#negated,
        timeoutMs: this.#timeout(opts),
        probe: async (limitMs) => {
          const n = await this.#countWithTimeout(limitMs)
          if (n === 0) return { kind: 'value', matched: false, actual: 'detached (count: 0)' }
          if (n === 1) return { kind: 'value', matched: true, actual: 'attached' }
          await this.#locator.isVisible({ timeout: limitMs })
          return { kind: 'value', matched: true, actual: `attached (count: ${n})` }
        },
      }),
    )
  }

  async toBeEnabled(options) {
    return this.#boolCheck({
      matcher: 'toBeEnabled',
      expected: 'enabled',
      trueLabel: 'enabled',
      falseLabel: 'disabled',
      requireAttached: true,
      options,
      fn: (limitMs) => this.#locator.isEnabled({ timeout: limitMs }),
    })
  }

  async toBeDisabled(options) {
    return this.#boolCheck({
      matcher: 'toBeDisabled',
      expected: 'disabled',
      trueLabel: 'disabled',
      falseLabel: 'enabled',
      requireAttached: true,
      options,
      fn: (limitMs) => this.#locator.isDisabled({ timeout: limitMs }),
    })
  }

  async toBeChecked(options) {
    const opts = validateOptions('expect(locator).toBeChecked', options, { checked: true })
    const wantChecked = opts.checked ?? true
    return this.#run(() =>
      pollUntil({
        targetLabel: formatLocator(this.#locator),
        matcher: 'toBeChecked',
        expectedDesc: wantChecked ? 'checked' : 'unchecked',
        negated: this.#negated,
        timeoutMs: this.#timeout(opts),
        probe: async (limitMs) => {
          const early = await this.#ensureAttached(limitMs)
          if (early) return early
          const val = await this.#locator.isChecked({ timeout: limitMs })
          return {
            kind: 'value',
            matched: val === wantChecked,
            actual: val ? 'checked' : 'unchecked',
          }
        },
      }),
    )
  }

  async toBeEditable(options) {
    return this.#boolCheck({
      matcher: 'toBeEditable',
      expected: 'editable',
      trueLabel: 'editable',
      falseLabel: 'not editable',
      requireAttached: true,
      options,
      fn: (limitMs) => this.#locator.isEditable({ timeout: limitMs }),
    })
  }

  async toHaveText(expected, options) {
    validatePattern(expected, 'expect(locator).toHaveText')
    const opts = validateOptions('expect(locator).toHaveText', options)
    return this.#run(() =>
      pollUntil({
        targetLabel: formatLocator(this.#locator),
        matcher: 'toHaveText',
        expectedDesc: formatExpected(expected, true),
        negated: this.#negated,
        timeoutMs: this.#timeout(opts),
        probe: async (limitMs) => {
          const early = await this.#ensureAttached(limitMs)
          if (early) return early
          const raw = (await this.#locator.textContent({ timeout: limitMs })) ?? ''
          const actualNorm = normalizeWhitespace(raw)
          return {
            kind: 'value',
            matched: matchPattern(raw, expected, { normalize: true, substring: false }),
            actual: JSON.stringify(actualNorm),
          }
        },
      }),
    )
  }

  async toContainText(expected, options) {
    validatePattern(expected, 'expect(locator).toContainText')
    const opts = validateOptions('expect(locator).toContainText', options)
    return this.#run(() =>
      pollUntil({
        targetLabel: formatLocator(this.#locator),
        matcher: 'toContainText',
        expectedDesc: `substring ${formatExpected(expected, true)}`,
        negated: this.#negated,
        timeoutMs: this.#timeout(opts),
        probe: async (limitMs) => {
          const early = await this.#ensureAttached(limitMs)
          if (early) return early
          const raw = (await this.#locator.textContent({ timeout: limitMs })) ?? ''
          const actualNorm = normalizeWhitespace(raw)
          return {
            kind: 'value',
            matched: matchPattern(raw, expected, { normalize: true, substring: true }),
            actual: JSON.stringify(actualNorm),
          }
        },
      }),
    )
  }

  async toHaveValue(expected, options) {
    validatePattern(expected, 'expect(locator).toHaveValue')
    const opts = validateOptions('expect(locator).toHaveValue', options)
    return this.#run(() =>
      pollUntil({
        targetLabel: formatLocator(this.#locator),
        matcher: 'toHaveValue',
        expectedDesc: formatExpected(expected, false),
        negated: this.#negated,
        timeoutMs: this.#timeout(opts),
        probe: async (limitMs) => {
          const early = await this.#ensureAttached(limitMs)
          if (early) return early
          const val = await this.#locator.inputValue({ timeout: limitMs })
          return {
            kind: 'value',
            matched: matchPattern(val, expected, { normalize: false, substring: false }),
            actual: JSON.stringify(val),
          }
        },
      }),
    )
  }

  async toHaveAttribute(name, expected, options) {
    if (typeof name !== 'string' || !name) {
      throw new TypeError('expect(locator).toHaveAttribute: name must be a non-empty string')
    }
    validatePattern(expected, 'expect(locator).toHaveAttribute')
    const opts = validateOptions('expect(locator).toHaveAttribute', options)
    return this.#run(() =>
      pollUntil({
        targetLabel: formatLocator(this.#locator),
        matcher: 'toHaveAttribute',
        expectedDesc: `${name}=${formatExpected(expected, false)}`,
        negated: this.#negated,
        timeoutMs: this.#timeout(opts),
        probe: async (limitMs) => {
          const early = await this.#ensureAttached(limitMs)
          if (early) return early
          const val = await this.#locator.getAttribute(name, { timeout: limitMs })
          if (val === null) {
            return { kind: 'value', matched: false, actual: `${name}=<absent>` }
          }
          return {
            kind: 'value',
            matched: matchPattern(val, expected, { normalize: false, substring: false }),
            actual: `${name}=${JSON.stringify(val)}`,
          }
        },
      }),
    )
  }

  async toHaveClass(expected, options) {
    validatePattern(expected, 'expect(locator).toHaveClass')
    const opts = validateOptions('expect(locator).toHaveClass', options)
    return this.#run(() =>
      pollUntil({
        targetLabel: formatLocator(this.#locator),
        matcher: 'toHaveClass',
        expectedDesc: formatExpected(expected, false),
        negated: this.#negated,
        timeoutMs: this.#timeout(opts),
        probe: async (limitMs) => {
          const early = await this.#ensureAttached(limitMs)
          if (early) return early
          const cls = (await this.#locator.getAttribute('class', { timeout: limitMs })) ?? ''
          return {
            kind: 'value',
            matched: matchPattern(cls, expected, { normalize: false, substring: false }),
            actual: JSON.stringify(cls),
          }
        },
      }),
    )
  }

  async toHaveCount(expected, options) {
    if (typeof expected !== 'number' || !Number.isInteger(expected) || expected < 0) {
      throw new TypeError('expect(locator).toHaveCount: count must be a non-negative integer')
    }
    const opts = validateOptions('expect(locator).toHaveCount', options)
    return this.#run(() =>
      pollUntil({
        targetLabel: formatLocator(this.#locator),
        matcher: 'toHaveCount',
        expectedDesc: String(expected),
        negated: this.#negated,
        timeoutMs: this.#timeout(opts),
        probe: async (limitMs) => {
          const n = await this.#countWithTimeout(limitMs)
          return { kind: 'value', matched: n === expected, actual: String(n) }
        },
      }),
    )
  }

  async toHaveScreenshot(nameOrPath, options) {
    if (this.#negated) {
      throw new Error('expect(locator).toHaveScreenshot: .not is not supported')
    }
    const ownerPage = typeof this.#getOwnerPage === 'function' ? this.#getOwnerPage(this.#locator) : null
    const opts = validateScreenshotCall(
      `${formatLocator(this.#locator)}.toHaveScreenshot`,
      nameOrPath,
      options,
      false,
      ownerPage,
      this.#getOwnerPage,
    )
    const timeoutMs = opts.timeout ?? this.#defaultTimeout
    const inFrame = typeof this.#locator._frames === 'function' && this.#locator._frames().length > 0
    return this.#run(() =>
      runToHaveScreenshot({
        targetLabel: formatLocator(this.#locator),
        nameOrPath,
        opts,
        timeoutMs,
        isClosed: () => Boolean(ownerPage && typeof ownerPage.isClosed === 'function' && ownerPage.isClosed()),
        disableAnimations: async (limitMs) => {
          if (ownerPage && !inFrame) {
            await ownerPage.evaluate(DISABLE_ANIMATIONS_PAGE_FN).catch(() => {})
            return
          }
          await this.#locator.evaluate(DISABLE_ANIMATIONS_LOCATOR_FN, undefined, { timeout: Math.min(limitMs, 500) }).catch(() => {})
          if (ownerPage) {
            await ownerPage.evaluate(DISABLE_ANIMATIONS_PAGE_FN).catch(() => {})
          }
        },
        restoreAnimations: async () => {
          if (ownerPage) {
            await ownerPage.evaluate(RESTORE_ANIMATIONS_PAGE_FN).catch(() => {})
            if (inFrame && typeof ownerPage.frames === 'function') {
              await Promise.allSettled(
                ownerPage.frames().map((f) => f.evaluate(RESTORE_ANIMATIONS_PAGE_FN).catch(() => {})),
              )
            }
            return
          }
          await this.#locator.evaluate(RESTORE_ANIMATIONS_LOCATOR_FN, undefined, { timeout: 250 }).catch(() => {})
        },
        captureShot: (limitMs) =>
          this.#locator.screenshot({
            omitBackground: opts.omitBackground,
            ...(opts.mask !== undefined ? { mask: opts.mask } : {}),
            ...(opts.maskColor !== undefined ? { maskColor: opts.maskColor } : {}),
            ...(opts.animations === 'allow' && opts.caret ? { caret: opts.caret } : {}),
            timeout: limitMs,
          }),
      }),
    )
  }
}

class PageAssertions {
  #page
  #defaultTimeout
  #negated
  #onSoftError
  #getOwnerPage

  constructor(page, defaultTimeout, negated = false, onSoftError = null, getOwnerPage = null) {
    this.#page = page
    this.#defaultTimeout = defaultTimeout
    this.#negated = negated
    this.#onSoftError = onSoftError
    this.#getOwnerPage = getOwnerPage
  }

  get not() {
    return new PageAssertions(this.#page, this.#defaultTimeout, !this.#negated, this.#onSoftError, this.#getOwnerPage)
  }

  #timeout(options) {
    return options?.timeout ?? this.#defaultTimeout
  }

  async #run(fn) {
    try {
      return await fn()
    } catch (err) {
      if (this.#onSoftError && !(err instanceof TypeError)) {
        this.#onSoftError(err)
        return
      }
      throw err
    }
  }

  async toHaveTitle(expected, options) {
    validatePattern(expected, 'expect(page).toHaveTitle')
    const opts = validateOptions('expect(page).toHaveTitle', options)
    return this.#run(() =>
      pollUntil({
        targetLabel: 'expect(page)',
        matcher: 'toHaveTitle',
        expectedDesc: formatExpected(expected, true),
        negated: this.#negated,
        timeoutMs: this.#timeout(opts),
        probe: async (limitMs) => {
          if (this.#page.isClosed()) throw new Error('page is closed')
          const raw = await raceTimeout(this.#page.title(), limitMs, 'page.title')
          const actualNorm = normalizeWhitespace(raw)
          return {
            kind: 'value',
            matched: matchPattern(raw, expected, { normalize: true, substring: false }),
            actual: JSON.stringify(actualNorm),
          }
        },
      }),
    )
  }

  async toHaveURL(expected, options) {
    validatePattern(expected, 'expect(page).toHaveURL')
    const opts = validateOptions('expect(page).toHaveURL', options)
    const target = resolveUrl(this.#page, expected)
    return this.#run(() =>
      pollUntil({
        targetLabel: 'expect(page)',
        matcher: 'toHaveURL',
        expectedDesc: formatExpected(target, false),
        negated: this.#negated,
        timeoutMs: this.#timeout(opts),
        probe: async () => {
          if (this.#page.isClosed()) throw new Error('page is closed')
          const actual = this.#page.url()
          return {
            kind: 'value',
            matched: matchPattern(actual, target, { normalize: false, substring: false }),
            actual: JSON.stringify(actual),
          }
        },
      }),
    )
  }

  async toHaveScreenshot(nameOrPath, options) {
    if (this.#negated) {
      throw new Error('expect(page).toHaveScreenshot: .not is not supported')
    }
    const opts = validateScreenshotCall(
      'expect(page).toHaveScreenshot',
      nameOrPath,
      options,
      true,
      this.#page,
      this.#getOwnerPage,
    )
    const timeoutMs = opts.timeout ?? this.#defaultTimeout
    return this.#run(() =>
      runToHaveScreenshot({
        targetLabel: 'expect(page)',
        nameOrPath,
        opts,
        timeoutMs,
        isClosed: () => this.#page.isClosed(),
        disableAnimations: () => this.#page.evaluate(DISABLE_ANIMATIONS_PAGE_FN).catch(() => {}),
        restoreAnimations: () => this.#page.evaluate(RESTORE_ANIMATIONS_PAGE_FN).catch(() => {}),
        captureShot: (limitMs) =>
          this.#page.screenshot({
            fullPage: opts.fullPage,
            clip: opts.clip,
            omitBackground: opts.omitBackground,
            ...(opts.mask !== undefined ? { mask: opts.mask } : {}),
            ...(opts.maskColor !== undefined ? { maskColor: opts.maskColor } : {}),
            ...(opts.animations === 'allow' && opts.caret ? { caret: opts.caret } : {}),
            timeout: limitMs,
          }),
      }),
    )
  }
}

function validateScreenshotCall(method, nameOrPath, options, isPage, page = null, getOwnerPage = null) {
  if (typeof nameOrPath === 'string') {
    if (!nameOrPath) throw new TypeError(`${method}: path must be a non-empty string`)
  } else if (Array.isArray(nameOrPath)) {
    if (!nameOrPath.length || nameOrPath.some((s) => typeof s !== 'string' || !s)) {
      throw new TypeError(`${method}: path segments must be non-empty strings`)
    }
  } else {
    throw new TypeError(`${method}: path must be a string or string[]`)
  }

  const allowed = {
    timeout: true,
    threshold: true,
    maxDiffPixels: true,
    maxDiffPixelRatio: true,
    animations: true,
    caret: true,
    mask: true,
    maskColor: true,
    update: true,
    omitBackground: true,
    ...(isPage ? { fullPage: true, clip: true } : {}),
  }
  const opts = validateOptions(method, options, allowed)
  const threshold = opts.threshold ?? 0.2
  if (typeof threshold !== 'number' || !Number.isFinite(threshold) || threshold < 0 || threshold > 1) {
    throw new TypeError(`${method}: threshold must be a finite number in [0, 1]`)
  }
  if (opts.maxDiffPixels != null) {
    if (typeof opts.maxDiffPixels !== 'number' || !Number.isInteger(opts.maxDiffPixels) || opts.maxDiffPixels < 0) {
      throw new TypeError(`${method}: maxDiffPixels must be a non-negative integer`)
    }
  }
  if (opts.maxDiffPixelRatio != null) {
    if (
      typeof opts.maxDiffPixelRatio !== 'number' ||
      !Number.isFinite(opts.maxDiffPixelRatio) ||
      opts.maxDiffPixelRatio < 0 ||
      opts.maxDiffPixelRatio > 1
    ) {
      throw new TypeError(`${method}: maxDiffPixelRatio must be a finite number in [0, 1]`)
    }
  }
  const animations = opts.animations ?? 'disabled'
  if (animations !== 'disabled' && animations !== 'allow') {
    throw new TypeError(`${method}: animations must be 'disabled' or 'allow'`)
  }
  if (opts.caret != null && opts.caret !== 'hide' && opts.caret !== 'initial') {
    throw new TypeError(`${method}: caret must be 'hide' or 'initial'`)
  }
  let resolvedMask
  if (opts.mask != null) {
    resolvedMask = resolveMaskSelectors(method, opts.mask, page, getOwnerPage)
  }
  if (opts.maskColor != null) {
    validateMaskColor(method, opts.maskColor)
  }
  for (const bKey of ['update', 'omitBackground', 'fullPage']) {
    if (opts[bKey] != null && typeof opts[bKey] !== 'boolean') {
      throw new TypeError(`${method}: ${bKey} must be a boolean`)
    }
  }
  if (opts.clip != null) {
    const clip = validateOptions(`${method} clip`, opts.clip, { x: true, y: true, width: true, height: true }, false)
    for (const k of ['x', 'y', 'width', 'height']) {
      if (typeof clip[k] !== 'number' || !Number.isFinite(clip[k])) {
        throw new TypeError(`${method}: clip.${k} must be a finite number`)
      }
    }
    if (clip.width <= 0 || clip.height <= 0) {
      throw new TypeError(`${method}: clip.width and clip.height must be positive`)
    }
  }
  artifactPaths(nameOrPath)
  return {
    timeout: opts.timeout,
    threshold,
    maxDiffPixels: opts.maxDiffPixels,
    maxDiffPixelRatio: opts.maxDiffPixelRatio,
    animations,
    caret: opts.caret,
    mask: resolvedMask,
    maskColor: opts.maskColor,
    update: opts.update,
    omitBackground: opts.omitBackground ?? false,
    fullPage: opts.fullPage ?? false,
    clip: opts.clip,
  }
}

async function writeArtifact(filePath, data) {
  await fs.mkdir(dirname(filePath), { recursive: true })
  await fs.writeFile(filePath, data)
}

async function runToHaveScreenshot({
  targetLabel,
  nameOrPath,
  opts,
  timeoutMs,
  isClosed,
  disableAnimations,
  restoreAnimations,
  captureShot,
}) {
  const { baselinePath, actualPath, diffPath } = artifactPaths(nameOrPath)
  const disableAnim = opts.animations === 'disabled'
  const unlimited = timeoutMs === 0
  const shotLimit = (remaining) => (!Number.isFinite(remaining) ? 2000 : Math.max(100, Math.min(remaining, 2000)))

  try {
    if (shouldUpdateSnapshots(opts.update)) {
      const start = Date.now()
      let prevImg = null
      let lastErr = null
      let attempts = 0
      while (true) {
        if (isClosed()) throw new Error('page is closed')
        const remaining = unlimited ? Infinity : Math.max(0, timeoutMs - (Date.now() - start))
        if (disableAnim) await disableAnimations(shotLimit(remaining))
        let stabilizedBytes = null
        try {
          const bytes = await captureShot(shotLimit(remaining))
          attempts++
          const img = decodePng(bytes)
          if (prevImg) {
            const diff = compareImages(prevImg, img, opts.threshold, false)
            if (isWithinTolerance(diff, opts)) {
              stabilizedBytes = bytes
            }
          }
          prevImg = img
          lastErr = null
        } catch (err) {
          if (isFatalError(err)) throw err
          lastErr = String(err?.message ?? err)
        }
        if (stabilizedBytes) {
          await writeArtifact(baselinePath, stabilizedBytes)
          return
        }

        const elapsed = Date.now() - start
        if (!unlimited && elapsed >= timeoutMs) {
          if (attempts === 1 && prevImg) {
            let finalBytes = null
            try {
              const bytes = await captureShot(1000)
              const img = decodePng(bytes)
              const diff = compareImages(prevImg, img, opts.threshold, false)
              if (isWithinTolerance(diff, opts)) {
                finalBytes = bytes
              }
            } catch {}
            if (finalBytes) {
              await writeArtifact(baselinePath, finalBytes)
              return
            }
          }
          throw new Error(
            `${targetLabel}.toHaveScreenshot: ${
              attempts >= 2
                ? `Timed out ${timeoutMs}ms waiting for screenshot to stabilize for update at ${baselinePath}`
                : `Failed to capture snapshot (${lastErr ?? 'did not stabilize'})`
            }`,
          )
        }
        const waitMs = unlimited ? POLL_INTERVAL_MS : Math.min(POLL_INTERVAL_MS, Math.max(0, timeoutMs - elapsed))
        if (waitMs > 0) await new Promise((r) => setTimeout(r, waitMs))
      }
    }

    let baselineBytes
    try {
      baselineBytes = await fs.readFile(baselinePath)
    } catch (err) {
      if (err?.code === 'ENOENT') {
        if (disableAnim) await disableAnimations(500)
        try {
          const actual = await captureShot(2000)
          await writeArtifact(actualPath, actual)
        } catch {}
        throw new Error(
          `${targetLabel}.toHaveScreenshot: Baseline snapshot does not exist at ${baselinePath}.\n` +
            `Run with FLUXWRIGHT_UPDATE_SNAPSHOTS=1 or { update: true } to create it. Actual saved to ${actualPath}`,
        )
      }
      throw err
    }

    const baselineImg = decodePng(baselineBytes)
    const start = Date.now()
    let lastMismatch = null
    let lastErr = null

    while (true) {
      if (isClosed()) throw new Error('page is closed')
      const remaining = unlimited ? Infinity : Math.max(0, timeoutMs - (Date.now() - start))
      if (disableAnim) await disableAnimations(shotLimit(remaining))
      try {
        const bytes = await captureShot(shotLimit(remaining))
        const actualImg = decodePng(bytes)
        const diff = compareImages(baselineImg, actualImg, opts.threshold, false)
        if (isWithinTolerance(diff, opts)) return
        lastMismatch = { bytes, actualImg }
        lastErr = null
      } catch (err) {
        if (isFatalError(err)) throw err
        lastErr = String(err?.message ?? err)
      }

      const elapsed = Date.now() - start
      if (!unlimited && elapsed >= timeoutMs) break
      const waitMs = unlimited ? POLL_INTERVAL_MS : Math.min(POLL_INTERVAL_MS, Math.max(0, timeoutMs - elapsed))
      if (waitMs > 0) await new Promise((r) => setTimeout(r, waitMs))
    }

    if (lastMismatch) {
      const finalDiff = compareImages(baselineImg, lastMismatch.actualImg, opts.threshold, true)
      await writeArtifact(actualPath, lastMismatch.bytes)
      if (finalDiff.diffPng) await writeArtifact(diffPath, finalDiff.diffPng)
      throw new Error(
        `${targetLabel}.toHaveScreenshot: Screenshot mismatch (${finalDiff.diffPixels} of ${finalDiff.totalPixels} pixels differ, ratio ${finalDiff.diffRatio.toFixed(6)}; ` +
          `expected ${finalDiff.expectedSize[0]}x${finalDiff.expectedSize[1]}, actual ${finalDiff.actualSize[0]}x${finalDiff.actualSize[1]}).\n` +
          `Baseline: ${baselinePath}\nActual:   ${actualPath}\nDiff:     ${diffPath}`,
      )
    }

    throw new Error(
      `${targetLabel}.toHaveScreenshot: Timed out ${timeoutMs}ms waiting for screenshot (${lastErr ?? 'unknown error'}).`,
    )
  } finally {
    if (disableAnim && !isClosed()) await restoreAnimations()
  }
}

let activeSoftErrorCollector = null

function setSoftErrorCollector(fn) {
  const prev = activeSoftErrorCollector
  activeSoftErrorCollector = typeof fn === 'function' ? fn : null
  return prev
}

function createExpect(PageClass, LocatorClass, defaultTimeout = DEFAULT_TIMEOUT_MS, getOwnerPage = null) {
  const recordSoftError = (err) => {
    if (activeSoftErrorCollector) {
      activeSoftErrorCollector(err)
    }
  }

  function expect(target) {
    if (PageClass && target instanceof PageClass) {
      return new PageAssertions(target, defaultTimeout, false, null, getOwnerPage)
    }
    if (LocatorClass && target instanceof LocatorClass) {
      return new LocatorAssertions(target, defaultTimeout, false, getOwnerPage, null)
    }
    throw new TypeError('expect: argument must be a Page or Locator')
  }

  expect.soft = (target) => {
    if (PageClass && target instanceof PageClass) {
      return new PageAssertions(target, defaultTimeout, false, recordSoftError, getOwnerPage)
    }
    if (LocatorClass && target instanceof LocatorClass) {
      return new LocatorAssertions(target, defaultTimeout, false, getOwnerPage, recordSoftError)
    }
    throw new TypeError('expect.soft: argument must be a Page or Locator')
  }

  expect.configure = (options) => {
    const opts = validateOptions('expect.configure', options)
    const nextTimeout = opts.timeout ?? defaultTimeout
    return createExpect(PageClass, LocatorClass, nextTimeout, getOwnerPage)
  }

  return expect
}

module.exports = {
  createExpect,
  setSoftErrorCollector,
  LocatorAssertions,
  PageAssertions,
  isFatalError,
  resolveUrl,
}
