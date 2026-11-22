'use strict'

const DEFAULT_TIMEOUT_MS = 5000
const POLL_INTERVAL_MS = 50
const MIN_PROBE_TIMEOUT_MS = 50
const MAX_PROBE_TIMEOUT_MS = 250

function normalizeWhitespace(s) {
  return String(s).replace(/\s+/g, ' ').trim()
}

function compileRegExp(re) {
  return new RegExp(re.source, re.flags.replace(/[gy]/g, ''))
}

function validatePattern(expected, method) {
  if (typeof expected !== 'string' && !(expected instanceof RegExp)) {
    throw new TypeError(`${method}: expected a string or RegExp`)
  }
}

function validateOptions(method, options, extraAllowed = {}) {
  if (options == null) return {}
  if (typeof options !== 'object' || Array.isArray(options)) {
    throw new TypeError(`${method}: options must be an object`)
  }
  const allowed = { timeout: true, ...extraAllowed }
  for (const [key, value] of Object.entries(options)) {
    if (value === undefined) continue
    if (!Object.hasOwn(allowed, key)) {
      throw new TypeError(`${method}: option '${key}' is not supported`)
    }
    if (key === 'timeout' && value != null) {
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
    /invalid selector:/i.test(msg)
  )
}

function resolveUrl(page, expected) {
  if (typeof expected !== 'string') return expected
  const baseUrl = typeof page._baseUrl === 'function' ? page._baseUrl() : null
  if (baseUrl != null && !/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(expected)) {
    if (expected.startsWith('/')) return new URL(baseUrl).origin + expected
    return new URL(expected, baseUrl).href
  }
  return expected
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

  constructor(locator, defaultTimeout, negated = false) {
    this.#locator = locator
    this.#defaultTimeout = defaultTimeout
    this.#negated = negated
  }

  get not() {
    return new LocatorAssertions(this.#locator, this.#defaultTimeout, !this.#negated)
  }

  #timeout(options) {
    return options?.timeout ?? this.#defaultTimeout
  }

  async #ensureAttached(limitMs) {
    try {
      const n = await this.#locator.count()
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
    return pollUntil({
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
    })
  }

  toBeVisible(options) {
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

  toBeHidden(options) {
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

  toBeAttached(options) {
    const opts = validateOptions('expect(locator).toBeAttached', options)
    return pollUntil({
      targetLabel: formatLocator(this.#locator),
      matcher: 'toBeAttached',
      expectedDesc: 'attached',
      negated: this.#negated,
      timeoutMs: this.#timeout(opts),
      probe: async (limitMs) => {
        const n = await this.#locator.count()
        if (n === 0) return { kind: 'value', matched: false, actual: 'detached (count: 0)' }
        if (n === 1) return { kind: 'value', matched: true, actual: 'attached' }
        await this.#locator.isVisible({ timeout: limitMs })
        return { kind: 'value', matched: true, actual: `attached (count: ${n})` }
      },
    })
  }

  toBeEnabled(options) {
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

  toBeDisabled(options) {
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

  toBeChecked(options) {
    const opts = validateOptions('expect(locator).toBeChecked', options, { checked: true })
    const wantChecked = opts.checked ?? true
    return pollUntil({
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
    })
  }

  toBeEditable(options) {
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

  toHaveText(expected, options) {
    validatePattern(expected, 'expect(locator).toHaveText')
    const opts = validateOptions('expect(locator).toHaveText', options)
    return pollUntil({
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
    })
  }

  toContainText(expected, options) {
    validatePattern(expected, 'expect(locator).toContainText')
    const opts = validateOptions('expect(locator).toContainText', options)
    return pollUntil({
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
    })
  }

  toHaveValue(expected, options) {
    validatePattern(expected, 'expect(locator).toHaveValue')
    const opts = validateOptions('expect(locator).toHaveValue', options)
    return pollUntil({
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
    })
  }

  toHaveAttribute(name, expected, options) {
    if (typeof name !== 'string' || !name) {
      throw new TypeError('expect(locator).toHaveAttribute: name must be a non-empty string')
    }
    validatePattern(expected, 'expect(locator).toHaveAttribute')
    const opts = validateOptions('expect(locator).toHaveAttribute', options)
    return pollUntil({
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
    })
  }

  toHaveClass(expected, options) {
    validatePattern(expected, 'expect(locator).toHaveClass')
    const opts = validateOptions('expect(locator).toHaveClass', options)
    return pollUntil({
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
    })
  }

  toHaveCount(expected, options) {
    if (typeof expected !== 'number' || !Number.isInteger(expected) || expected < 0) {
      throw new TypeError('expect(locator).toHaveCount: count must be a non-negative integer')
    }
    const opts = validateOptions('expect(locator).toHaveCount', options)
    return pollUntil({
      targetLabel: formatLocator(this.#locator),
      matcher: 'toHaveCount',
      expectedDesc: String(expected),
      negated: this.#negated,
      timeoutMs: this.#timeout(opts),
      probe: async () => {
        const n = await this.#locator.count()
        return { kind: 'value', matched: n === expected, actual: String(n) }
      },
    })
  }
}

class PageAssertions {
  #page
  #defaultTimeout
  #negated

  constructor(page, defaultTimeout, negated = false) {
    this.#page = page
    this.#defaultTimeout = defaultTimeout
    this.#negated = negated
  }

  get not() {
    return new PageAssertions(this.#page, this.#defaultTimeout, !this.#negated)
  }

  #timeout(options) {
    return options?.timeout ?? this.#defaultTimeout
  }

  toHaveTitle(expected, options) {
    validatePattern(expected, 'expect(page).toHaveTitle')
    const opts = validateOptions('expect(page).toHaveTitle', options)
    return pollUntil({
      targetLabel: 'expect(page)',
      matcher: 'toHaveTitle',
      expectedDesc: formatExpected(expected, true),
      negated: this.#negated,
      timeoutMs: this.#timeout(opts),
      probe: async () => {
        if (this.#page.isClosed()) throw new Error('page is closed')
        const raw = await this.#page.title()
        const actualNorm = normalizeWhitespace(raw)
        return {
          kind: 'value',
          matched: matchPattern(raw, expected, { normalize: true, substring: false }),
          actual: JSON.stringify(actualNorm),
        }
      },
    })
  }

  toHaveURL(expected, options) {
    validatePattern(expected, 'expect(page).toHaveURL')
    const opts = validateOptions('expect(page).toHaveURL', options)
    const target = resolveUrl(this.#page, expected)
    return pollUntil({
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
    })
  }
}

function createExpect(PageClass, LocatorClass, defaultTimeout = DEFAULT_TIMEOUT_MS) {
  function expect(target) {
    if (PageClass && target instanceof PageClass) {
      return new PageAssertions(target, defaultTimeout, false)
    }
    if (LocatorClass && target instanceof LocatorClass) {
      return new LocatorAssertions(target, defaultTimeout, false)
    }
    throw new TypeError('expect: argument must be a Page or Locator')
  }

  expect.configure = (options) => {
    const opts = validateOptions('expect.configure', options)
    const nextTimeout = opts.timeout ?? defaultTimeout
    return createExpect(PageClass, LocatorClass, nextTimeout)
  }

  return expect
}

module.exports = { createExpect, LocatorAssertions, PageAssertions }
