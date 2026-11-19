'use strict'

const { EventEmitter } = require('node:events')
const fs = require('node:fs/promises')
const { dirname } = require('node:path')
const { Script } = require('node:vm')

// Like Playwright, `{ path }` creates missing folders (`.auth/state.json`).
async function save(path, data) {
  await fs.mkdir(dirname(path), { recursive: true })
  await fs.writeFile(path, data)
}

// `save` for saved cookies: new folders and the file are the owner's alone. Windows has no mode
// bits; there a new file takes its folder's access list.
async function savePrivate(path, data) {
  await fs.mkdir(dirname(path), { recursive: true, mode: 0o700 })
  const file = await fs.open(path, 'w', 0o600)
  try {
    // A file that already existed keeps its mode on open: narrow it before the cookies go in.
    await file.chmod(0o600)
    await file.writeFile(data)
  } finally {
    await file.close()
  }
}

// The options each method implements: a key maps to true (any value the native side accepts) or
// to the values it supports. Any other throws a TypeError. napi would ignore it, and Playwright
// code relying on it (`state: 'hidden'`, `type: 'jpeg'`, `button: 'right'`) would quietly do
// something else. MISSING.md lists what is not implemented.
const NONE = {}
const EXACT = { exact: true }
const STATE = { state: ['attached', 'detached', 'visible', 'hidden'] }
const TIMEOUT = { timeout: true }
const SHOT = { path: true, type: ['png', 'jpeg'], quality: true, omitBackground: true, timeout: true }
const PDF = {
  path: true,
  format: true,
  width: true,
  height: true,
  margin: true,
  landscape: true,
  printBackground: true,
  scale: true,
  pageRanges: true,
  preferCSSPageSize: true,
  displayHeaderFooter: true,
  headerTemplate: true,
  footerTemplate: true,
  outline: true,
  tagged: true,
}
const LAUNCH = {
  maxBrowsers: true,
  executablePath: true,
  headless: true,
  queueTimeout: true,
  memoryCeilingMb: true,
  maxConcurrencyPerHost: true,
  minDelayPerHost: true,
}
const PAGE = {
  proxy: true,
  userAgent: true,
  locale: true,
  timezoneId: true,
  geolocation: true,
  permissions: true,
  viewport: true,
  deviceScaleFactor: true,
  colorScheme: true,
  isMobile: true,
  hasTouch: true,
  extraHTTPHeaders: true,
  ignoreHTTPSErrors: true,
  javaScriptEnabled: true,
  offline: true,
  baseURL: true,
  httpCredentials: { username: true, password: true },
  bypassCSP: true,
  storageState: true,
}
const FULFILL = { status: true, headers: true, contentType: true, body: true, json: true, path: true, response: true }
const CONTINUE = { url: true, method: true, headers: true, postData: true }
const ROUTE_FETCH = { url: true, method: true, headers: true, postData: true, maxRedirects: true, timeout: true }
const API_REQ = { method: true, headers: true, params: true, data: true, form: true, multipart: true, failOnStatusCode: true, maxRedirects: true, timeout: true }
const BUTTON = ['left', 'right', 'middle']
const MODIFIER_NAMES = ['Alt', 'Control', 'Meta', 'Shift', 'ControlOrMeta']
const MODIFIERS = (v, m) => {
  if (!Array.isArray(v) || v.some((x) => !MODIFIER_NAMES.includes(x))) {
    throw new TypeError(`${m}: modifiers must be ${MODIFIER_NAMES.join(', ')}`)
  }
}
const POINT = (v, m) => {
  checkOptions(m, v, { x: true, y: true })
  if (typeof v?.x !== 'number' || typeof v?.y !== 'number') {
    throw new TypeError(`${m}: x and y must be numbers`)
  }
}
const CLICK = {
  button: BUTTON,
  clickCount: true,
  delay: true,
  position: POINT,
  modifiers: MODIFIERS,
  force: true,
  trial: true,
  timeout: true,
}
const DBLCLICK = {
  button: BUTTON,
  delay: true,
  position: POINT,
  modifiers: MODIFIERS,
  force: true,
  trial: true,
  timeout: true,
}
const HOVER = { position: POINT, modifiers: MODIFIERS, force: true, trial: true, timeout: true }
const CHECK = { position: POINT, force: true, trial: true, timeout: true }
const PRESS = { delay: true, timeout: true }
const FORCE_TIMEOUT = { force: true, timeout: true }
const MOUSE_MOVE = { steps: true }
const MOUSE_BUTTON = { button: BUTTON, clickCount: true }
const MOUSE_CLICK = { button: BUTTON, clickCount: true, delay: true }
const MOUSE_DBLCLICK = { button: BUTTON, delay: true }

const FILTER = { has: true, hasNot: true, hasText: true, hasNotText: true }
const ROLE = {
  name: true,
  exact: true,
  checked: true,
  disabled: true,
  expanded: true,
  includeHidden: true,
  level: true,
  pressed: true,
  selected: true,
}
const STRICT_TIMEOUT = { ...TIMEOUT, strict: true }
const NAV_OPTIONS = { waitUntil: true, timeout: true }

function checkOptions(method, options, allowed) {
  if (options == null) return
  if (typeof options !== 'object') throw new TypeError(`${method}: options must be an object`)
  for (const [key, value] of Object.entries(options)) {
    if (value == null) continue
    const ok = Object.hasOwn(allowed, key) && allowed[key]
    if (!ok) throw new TypeError(`${method}: option '${key}' is not supported (see MISSING.md)`)
    // NaN reaches napi as 0: an unset env var would make `maxBrowsers: 0` or no memory budget.
    if (Number.isNaN(value)) throw new TypeError(`${method}: option '${key}' is NaN`)
    if (typeof ok === 'function') {
      ok(value, `${method} ${key}`)
    } else if (Array.isArray(ok) && !ok.includes(value)) {
      const supported = ok.map((v) => JSON.stringify(v)).join(', ')
      throw new TypeError(`${method}: ${key} ${JSON.stringify(value)} is not supported, only ${supported} (see MISSING.md)`)
    } else if (typeof ok === 'object' && !Array.isArray(ok)) {
      checkOptions(`${method} ${key}`, value, ok)
    }
  }
}

// Checks the options argument (at index `at`) of native methods first. Async ones reject rather
// than throw, as the native call would.
function guard(Class, label, sync, methods) {
  for (const [name, at, allowed, via] of methods) {
    const call = Class.prototype[name]
    Class.prototype[name] = sync
      ? function (...args) {
          checkOptions(`${label}.${name}`, args[at], allowed)
          return call.apply(this, args)
        }
      : async function (...args) {
          checkOptions(`${label}.${name}`, args[at], allowed)
          // Playwright's per-call timeout: a locator carries it, and a page method that takes
          // a selector is the same action on that selector's locator.
          const timeout = args[at]?.timeout
          if (via) {
            const opts = args[at]
            const loc = this.locator(args[0])._strict(opts?.strict ?? false)
            const forwarded = opts && Object.hasOwn(opts, 'strict')
              ? (({ strict: _, ...rest }) => rest)(opts)
              : opts
            return args.length > at
              ? loc[via](...args.slice(1, at), forwarded)
              : loc[via](...args.slice(1))
          }
          if (timeout != null && this._withTimeout) return call.apply(this._withTimeout(timeout), args)
          return call.apply(this, args)
        }
  }
}

// An answer that comes after its handler returned, once the request went on or the dialog was
// answered for it: it resolves, with a warning, instead of rejecting where nothing catches it,
// which would crash Node.
function late(what) {
  console.warn(
    `[fluxwright] a ${what} was answered after its handler returned, and the answer is ignored: ` +
      'await it, or return its promise, inside the handler',
  )
  return Promise.resolve()
}

// Playwright-style `page.evaluate(fn, arg)`. As in Playwright, the function is sent to the page as
// source text, so it can use `arg` but not variables from Node. Strings run as expressions.
function toExpression(pageFunction, arg) {
  if (typeof pageFunction !== 'function') return pageFunction
  return `(${functionSource(pageFunction)})(${arg === undefined ? '' : JSON.stringify(arg)})`
}

// Object and class methods stringify as `name(x) { ... }`, which is not an expression on its own.
// Like Playwright, turn them into function expressions (this also covers async and generator methods).
function functionSource(fn) {
  let source = String(fn)
  if (!parses(source)) {
    source = source.startsWith('async ') ? `async function ${source.slice(6)}` : `function ${source}`
    if (!parses(source)) {
      throw new TypeError(`evaluate: this function can't be sent to the page: ${String(fn).slice(0, 60)}`)
    }
  }
  return source
}

function toError(e) {
  const err = new Error(e.message)
  if (e.name) err.name = e.name
  err.stack = e.stack
  return err
}

// Every listener of `event`, as emit runs them, except that one which throws or rejects is
// logged instead of reaching the native callback, where napi ends the process and every job in
// it (Playwright's `msg.text()` on the plain `{ type, text }` console messages did that).
function emitSafely(em, event, arg) {
  const failed = (err) => console.error(`[fluxwright] a page.on('${event}') listener failed:`, err)
  for (const listener of em?.rawListeners(event) ?? []) {
    try {
      const out = listener.call(em, arg)
      if (typeof out?.then === 'function') out.then(undefined, failed)
    } catch (err) {
      failed(err)
    }
  }
}

// Why a request failed, for request.failure(); set on requestfailed.
const failures = new WeakMap()

// Playwright's Request and Response over the native objects.
function toRequest(native) {
  const request = {
    url: () => native.url,
    method: () => native.method,
    headers: () => ({ ...native.headers }),
    postData: () => native.postData ?? null,
    resourceType: () => native.resourceType,
    failure: () => (failures.has(request) ? { errorText: failures.get(request) } : null),
  }
  return request
}

function toResponse(native, request) {
  const text = async () => (await native.body()).toString('utf8')
  return {
    url: () => native.url,
    status: () => native.status,
    statusText: () => native.statusText,
    ok: () => native.status >= 200 && native.status <= 299,
    headers: () => ({ ...native.headers }),
    request: () => request,
    body: () => native.body(),
    text,
    json: async () => JSON.parse(await text()),
  }
}

// Playwright's Dialog over the native one. Every dialog listener runs, and their promises are
// awaited. A dialog none of them answered is dismissed, so the page never hangs on it; a
// beforeunload one is accepted, as with no listener at all (dismissing it would cancel the
// navigation). An answer after that is too late.
async function answerDialog(listeners, native) {
  let answered = false
  let settled = false
  const answer = (work) => {
    if (settled) return late('dialog')
    answered = true
    return work()
  }
  const dialog = {
    type: () => native.type,
    message: () => native.message,
    defaultValue: () => native.defaultValue,
    accept: (promptText) => answer(() => native.accept(promptText)),
    dismiss: () => answer(() => native.dismiss()),
  }
  for (const r of await Promise.allSettled(listeners.map(async (handler) => handler(dialog)))) {
    if (r.status === 'rejected') console.error('[fluxwright] a dialog listener failed:', r.reason)
  }
  settled = true
  if (!answered) await (native.type === 'beforeunload' ? native.accept() : native.dismiss()).catch(() => {})
}

const popupWired = new WeakSet()
const pageContext = new WeakMap()
const contextRouteGetters = new WeakMap()
const pagesByTarget = new Map()
const contextRegistry = new FinalizationRegistry(({ browser, id }) => {
  browser._dropContext(id)
})
const frameIds = new WeakMap()
const cdpSessionRegistry = new FinalizationRegistry(({ page, id }) => {
  page._cdpDrop(id)
})

class CDPSession {
  #page
  #id
  #emitter = new EventEmitter()
  #started = false
  #detached = false

  constructor(page, id) {
    this.#page = page
    this.#id = id
    this.#emitter.setMaxListeners(0)
    cdpSessionRegistry.register(this, { page, id }, this)
  }

  #ensureEvents() {
    if (this.#started) return
    this.#started = true
    const ref = new WeakRef(this)
    this.#page._cdpOnEvent(this.#id, (ev) => {
      const s = ref.deref()
      if (!s || s.#detached) return
      let params = {}
      try {
        params = ev.paramsJson ? JSON.parse(ev.paramsJson) : {}
      } catch {}
      emitSafely(s.#emitter, ev.method, params)
    })
  }

  async send(method, params) {
    if (this.#detached) throw new Error('cdpSession.send: session is detached')
    if (typeof method !== 'string' || !method) {
      throw new TypeError('cdpSession.send: method must be a non-empty string')
    }
    const paramsJson = params === undefined ? undefined : JSON.stringify(params)
    return this.#page._cdpSend(this.#id, method, paramsJson)
  }

  on(event, handler) {
    this.#ensureEvents()
    this.#emitter.on(event, handler)
    return this
  }

  once(event, handler) {
    this.#ensureEvents()
    this.#emitter.once(event, handler)
    return this
  }

  addListener(event, handler) {
    return this.on(event, handler)
  }

  off(event, handler) {
    this.#emitter.off(event, handler)
    return this
  }

  removeListener(event, handler) {
    return this.off(event, handler)
  }

  async detach() {
    if (this.#detached) return
    this.#detached = true
    cdpSessionRegistry.unregister(this)
    this.#emitter.removeAllListeners()
    await this.#page._cdpDetach(this.#id)
  }
}

function trackTarget(page) {
  const tid = page._targetId()
  pagesByTarget.set(tid, new WeakRef(page))
  return tid
}

// page.on / once / off as in Playwright, for console messages, page errors, network events,
// dialogs, file choosers and popups. Each source starts with its first listener.
function installEvents(Page) {
  const emitters = new WeakMap()
  const sources = {
    console: 'log',
    pageerror: 'log',
    request: 'network',
    response: 'network',
    requestfinished: 'network',
    requestfailed: 'network',
    dialog: 'dialog',
    filechooser: 'filechooser',
    popup: 'popup',
  }
  const events = Object.keys(sources)
  // Pages whose close() was called: the native flag goes up only once napi runs the call.
  const closing = new WeakSet()

  function ensurePopupSubscription(page) {
    trackTarget(page)
    if (popupWired.has(page)) return
    popupWired.add(page)
    const ref = new WeakRef(page)
    page._onPopup((popup) => {
      const opener = ref.deref()
      trackTarget(popup)
      const ctx = opener ? pageContext.get(opener)?.deref() : undefined
      if (ctx) ctx._adoptPopup(popup)
      if (opener) emitSafely(emitters.get(opener)?.em, 'popup', popup)
    })
  }

  // The native side keeps these callbacks as long as the page's lease. They reach the emitter only
  // through a WeakRef and the WeakMap, so listeners that use the page don't keep a forgotten page,
  // and its browser slot, alive. Made apart from `stateOf`, whose scope holds the page.
  function subscribe(page, source) {
    const ref = new WeakRef(page)
    const em = () => emitters.get(ref.deref())?.em
    if (source === 'log') {
      page._onLog((e) =>
        e.event === 'console' ? emitSafely(em(), 'console', e.message) : emitSafely(em(), 'pageerror', toError(e.error)),
      )
    } else if (source === 'network') {
      // One Request per request, as in Playwright: its request, response, requestfinished and
      // requestfailed events, and response.request(), hand over the same object.
      const requests = new Map()
      page._onNetwork((e) => {
        let request = requests.get(e.request.id)
        if (!request) {
          request = toRequest(e.request)
          requests.set(e.request.id, request)
          // Ones that never finish (a long poll, say) go in the end, as in the engine.
          if (requests.size > 1000) requests.delete(requests.keys().next().value)
        }
        if (e.event === 'requestfinished' || e.event === 'requestfailed') requests.delete(e.request.id)
        if (e.event === 'requestfailed') failures.set(request, e.failure)
        emitSafely(em(), e.event, e.event === 'response' ? toResponse(e.response, request) : request)
      })
    } else if (source === 'filechooser') {
      page._onFileChooser((ev) => {
        const p = ref.deref()
        if (!p) return
        const fc = {
          page: () => p,
          isMultiple: () => ev.isMultiple,
          setFiles: async (files, options) => {
            checkOptions('fileChooser.setFiles', options, TIMEOUT)
            await applyFiles(
              {
                paths: (ps) => p._setFileChooserPaths(ev.id, ps, options?.timeout),
                payloads: (fs) => p._setFileChooserPayloads(ev.id, fs, options?.timeout),
              },
              'paths',
              'payloads',
              files,
            )
          },
        }
        emitSafely(em(), 'filechooser', fc)
      })
    } else if (source === 'popup') {
      ensurePopupSubscription(page)
    } else {
      page._onDialog((native) => answerDialog(em()?.rawListeners('dialog') ?? [], native))
    }
  }
  // The page's emitter and its pending waits, which close() rejects; starts `event`'s source.
  const stateOf = (page, event) => {
    let state = emitters.get(page)
    if (!state) {
      const em = new EventEmitter()
      em.setMaxListeners(0) // waitForResponse adds one per call
      state = { em, started: new Set(), waits: new Set() }
      emitters.set(page, state)
    }
    const source = sources[event]
    if (!state.started.has(source)) {
      subscribe(page, source)
      state.started.add(source)
    }
    return state
  }
  for (const method of ['on', 'once', 'addListener']) {
    Page.prototype[method] = function (event, handler) {
      if (!events.includes(event)) throw new Error(`page.${method}: '${event}' is not supported (${events.join(', ')})`)
      stateOf(this, event).em[method === 'addListener' ? 'on' : method](event, handler)
      return this
    }
  }
  for (const method of ['off', 'removeListener']) {
    Page.prototype[method] = function (event, handler) {
      emitters.get(this)?.em.off(event, handler)
      return this
    }
  }
  function waitForEvent(page, method, event, allowed, options, makeMatch) {
    return new Promise((resolve, reject) => {
      checkOptions(`page.${method}`, options, allowed)
      const matches = makeMatch()
      if (closing.has(page) || page.isClosed()) throw new Error(`page.${method}: the page is closed`)
      const timeout = options?.timeout ?? 30000
      const { em, waits } = stateOf(page, event)
      let timer = null
      const fail = (err) => {
        done()
        reject(err)
      }
      const closed = () => fail(new Error(`page.${method}: the page closed`))
      const done = () => {
        em.off(event, listener)
        clearTimeout(timer)
        waits.delete(closed)
      }
      const listener = async (r) => {
        try {
          if (!(await matches(r))) return
        } catch (err) {
          return fail(err)
        }
        done()
        resolve(r)
      }
      em.on(event, listener)
      waits.add(closed)
      if (timeout > 0) {
        timer = setTimeout(() => fail(new Error(`page.${method}: timeout ${timeout}ms exceeded`)), timeout)
      }
    })
  }
  // page.waitForResponse / waitForRequest / waitForFileChooser: the first match from now on, within
  // `timeout` ms (default 30 s, 0 for none). Closing the page rejects it.
  for (const [method, event] of [
    ['waitForResponse', 'response'],
    ['waitForRequest', 'request'],
  ]) {
    Page.prototype[method] = function (urlOrPredicate, options) {
      return waitForEvent(this, method, event, TIMEOUT, options, () => {
        if (typeof urlOrPredicate === 'function') return urlOrPredicate
        const url = urlMatcher(urlOrPredicate)
        return (r) => url(r.url())
      })
    }
  }
  Page.prototype.waitForFileChooser = function (options) {
    return waitForEvent(this, 'waitForFileChooser', 'filechooser', { timeout: true, predicate: true }, options, () => {
      if (options?.predicate != null && typeof options.predicate !== 'function') {
        throw new TypeError('page.waitForFileChooser: predicate must be a function')
      }
      return options?.predicate ?? (() => true)
    })
  }
  Page.prototype.waitForPopup = function (options) {
    return waitForEvent(this, 'waitForPopup', 'popup', { timeout: true, predicate: true }, options, () => {
      if (options?.predicate != null && typeof options.predicate !== 'function') {
        throw new TypeError('page.waitForPopup: predicate must be a function')
      }
      return options?.predicate ?? (() => true)
    })
  }
  Page.prototype.opener = async function () {
    const id = this._openerTargetId()
    if (!id) return null
    const p = pagesByTarget.get(id)?.deref()
    return p && !p.isClosed() ? p : null
  }
  const origIsClosed = Page.prototype.isClosed
  Page.prototype.isClosed = function () {
    return closing.has(this) || origIsClosed.call(this)
  }
  // As in Playwright, closing the page rejects its pending waits at once; calls still running on
  // the page reject on the native side.
  const close = Page.prototype.close
  Page.prototype.close = async function (options) {
    checkOptions('page.close', options, NONE)
    closing.add(this)
    pagesByTarget.delete(this._targetId())
    pageContext.get(this)?.deref()?._removePage(this)
    for (const closed of emitters.get(this)?.waits ?? []) closed()
    return close.call(this)
  }
  return ensurePopupSubscription
}

// Playwright's URL patterns: a glob string (** any characters, * any but /, {a,b} either), a
// RegExp, or a function of the URL.
function urlMatcher(pattern) {
  if (typeof pattern === 'function') return (url) => pattern(new URL(url))
  // A copy without g and y, whose lastIndex would make repeated tests alternate.
  if (pattern instanceof RegExp) {
    const regex = new RegExp(pattern.source, pattern.flags.replace(/[gy]/g, ''))
    return (url) => regex.test(url)
  }
  let re = ''
  let inGroup = false
  const glob = String(pattern)
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i]
    if (c === '*') {
      const deep = glob[i + 1] === '*'
      re += deep ? '.*' : '[^/]*'
      if (deep) i++
    } else if (c === '{') {
      inGroup = true
      re += '(?:'
    } else if (c === '}' && inGroup) {
      inGroup = false
      re += ')'
    } else if (c === ',' && inGroup) {
      re += '|'
    } else {
      re += /[\\^$+.()|[\]{}?]/.test(c) ? '\\' + c : c
    }
  }
  const regex = new RegExp('^' + re + '$')
  return (url) => regex.test(url)
}

// page.route / unroute with Playwright's Route and Request. The handler added last runs first;
// route.fallback() passes the request to the next matching one, and unmatched requests continue.
async function dispatchRoutes(list, extra, native) {
  const request = toRequest(native)
  let answered = false
  try {
    for (const handlers of [list, extra]) {
      for (let i = handlers.length - 1; i >= 0; i--) {
        // A URL function is user code: one that throws counts as no match.
        let matched = false
        try {
          matched = handlers[i].matches(native.url)
        } catch (err) {
          console.error('[fluxwright] a page.route URL matcher failed:', err)
        }
        if (!matched) continue
        // The first answer wins; it is set when the call starts, so an answer still under way
        // when the handler returns is waited for, not taken as no answer. One that comes after
        // the handler returned is too late: the request went on without it.
        let answer = null
        let fellBack = false
        let returned = false
        const answerWith = (work) => {
          if (returned) return late('route')
          if (answer || fellBack) return Promise.reject(new Error('route is already handled'))
          answer = work()
          return answer
        }
        const route = {
          request: () => request,
          fetch: async (o = {}) => {
            if (returned) return late('route')
            if (answer || fellBack) throw new Error('route is already handled')
            checkOptions('route.fetch', o, ROUTE_FETCH)
            let postData = o.postData
            if (postData !== undefined && !Buffer.isBuffer(postData) && typeof postData !== 'string') {
              postData = JSON.stringify(postData)
            }
            const raw = await native.fetch({
              url: o.url,
              method: o.method,
              headers: o.headers,
              postData,
              maxRedirects: o.maxRedirects,
              timeoutMs: o.timeout,
            })
            return new APIResponse(raw)
          },
          fulfill: (o = {}) =>
            answerWith(async () => {
              checkOptions('route.fulfill', o, FULFILL)
              let status = o.status
              let headers = {}
              let body = o.body
              if (o.response) {
                status ??= o.response.status()
                const skip = new Set(['content-encoding', 'content-length', 'transfer-encoding', 'connection'])
                for (const h of o.response.headersArray()) {
                  const k = h.name.toLowerCase()
                  if (skip.has(k)) continue
                  headers[k] = k in headers ? `${headers[k]}${k === 'set-cookie' ? '\n' : ', '}${h.value}` : h.value
                }
                if (o.headers) {
                  for (const [k, v] of Object.entries(o.headers)) {
                    for (const ek of Object.keys(headers)) {
                      if (ek.toLowerCase() === k.toLowerCase()) delete headers[ek]
                    }
                    headers[k] = v
                  }
                }
                if (o.json !== undefined) {
                  body = JSON.stringify(o.json)
                  headers['content-type'] ??= 'application/json'
                } else if (o.path) {
                  body = await fs.readFile(o.path)
                } else if (body === undefined) {
                  body = await o.response.body()
                }
              } else {
                headers = { ...o.headers }
                if (o.json !== undefined) {
                  body = JSON.stringify(o.json)
                  headers['content-type'] ??= 'application/json'
                }
                if (o.path) body = await fs.readFile(o.path)
              }
              if (o.contentType) headers['content-type'] = o.contentType
              return native.fulfill({ status, headers, body })
            }),
          continue: (o = {}) =>
            answerWith(async () => {
              checkOptions('route.continue', o, CONTINUE)
              return native.continue({ url: o.url, method: o.method, headers: o.headers, postData: o.postData })
            }),
          abort: (errorCode) => answerWith(() => native.abort(errorCode)),
          fallback: async (o) => {
            if (returned) return late('route')
            checkOptions('route.fallback', o, NONE)
            if (answer) throw new Error('route is already handled')
            fellBack = true
          },
        }
        try {
          await handlers[i].handler(route, request)
          returned = true
          if (fellBack && !answer) continue
          if (answer) {
            await answer
            answered = true
          }
        } catch (err) {
          console.error('[fluxwright] a route handler failed; the request continues:', err)
        } finally {
          returned = true
        }
        return
      }
    }
  } finally {
    // No answer, or one that failed: let the request through rather than hang the page.
    if (!answered) await native.continue().catch(() => {})
  }
}

function installRoutes(Page) {
  const routes = new WeakMap()
  // The native side keeps the dispatcher as long as the page's lease. It reaches the route list
  // through a WeakRef, so handlers that use the page don't keep a forgotten page alive. Made apart
  // from route(), whose scope holds the page.
  const dispatcher = (page) => {
    const ref = new WeakRef(page)
    return (native) => {
      const p = ref.deref()
      const list = p ? routes.get(p) ?? [] : []
      const extra = p ? contextRouteGetters.get(p)?.() ?? [] : []
      return dispatchRoutes(list, extra, native)
    }
  }
  // One interception start per page. Every route() call waits for it, so none returns before
  // requests are handed over, and a call adds its handler only once the start succeeded (a
  // failed start is tried again by the next call).
  const starts = new WeakMap()
  Page.prototype.route = async function (url, handler, options) {
    checkOptions('page.route', options, NONE)
    let list = routes.get(this)
    if (!list) {
      list = []
      routes.set(this, list)
    }
    let start = starts.get(this)
    if (!start) {
      this._setRouteHandler(dispatcher(this))
      start = this._intercept()
      starts.set(this, start)
      start.catch(() => starts.get(this) === start && starts.delete(this))
    }
    await start
    list.push({ url, handler, matches: urlMatcher(url) })
  }
  Page.prototype.unroute = async function (url, handler) {
    const list = routes.get(this) || []
    for (let i = list.length - 1; i >= 0; i--) {
      if (list[i].url === url && (!handler || list[i].handler === handler)) list.splice(i, 1)
    }
  }
}

function hasHeaderCI(headers, name) {
  const lower = name.toLowerCase()
  return Object.keys(headers).some((k) => k.toLowerCase() === lower)
}

function encodeQueryEntries(params) {
  if (params == null) return undefined
  if (typeof params === 'string') {
    return [...new URLSearchParams(params.startsWith('?') ? params.slice(1) : params).entries()].map(([name, value]) => ({ name, value }))
  }
  if (params instanceof URLSearchParams) {
    return [...params.entries()].map(([name, value]) => ({ name, value }))
  }
  if (typeof params === 'object') {
    return Object.entries(params).map(([name, value]) => ({ name, value: String(value) }))
  }
  throw new TypeError('apiRequestContext.fetch: params must be an object, URLSearchParams, or string')
}

async function encodeMultipartBody(multipart) {
  const boundary = `----FluxwrightBoundary${Date.now().toString(16)}${Math.random().toString(16).slice(2, 10)}`
  const chunks = []
  const push = (s) => chunks.push(typeof s === 'string' ? Buffer.from(s, 'utf8') : s)
  const cleanParam = (s) => String(s).replace(/[\r\n"]/g, '')
  const addField = async (name, value) => {
    push(`--${boundary}\r\n`)
    if (value && typeof value === 'object' && ('buffer' in value || typeof value.arrayBuffer === 'function' || typeof value.pipe === 'function')) {
      let filename = value.name ?? 'file'
      let mime = value.mimeType || value.type || 'application/octet-stream'
      let buf
      if (Buffer.isBuffer(value.buffer)) {
        buf = value.buffer
      } else if (typeof value.arrayBuffer === 'function') {
        buf = Buffer.from(await value.arrayBuffer())
      } else if (typeof value.pipe === 'function') {
        if (typeof value.path === 'string' && !value.name) filename = require('node:path').basename(value.path)
        const parts = []
        for await (const chunk of value) parts.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))
        buf = Buffer.concat(parts)
      } else {
        buf = Buffer.from(value.buffer)
      }
      push(`Content-Disposition: form-data; name="${cleanParam(name)}"; filename="${cleanParam(filename)}"\r\nContent-Type: ${mime}\r\n\r\n`)
      push(buf)
      push('\r\n')
    } else {
      push(`Content-Disposition: form-data; name="${cleanParam(name)}"\r\n\r\n`)
      push(String(value))
      push('\r\n')
    }
  }
  if (typeof FormData !== 'undefined' && multipart instanceof FormData) {
    for (const [k, v] of multipart.entries()) await addField(k, v)
  } else if (multipart && typeof multipart === 'object') {
    for (const [k, v] of Object.entries(multipart)) await addField(k, v)
  } else {
    throw new TypeError('apiRequestContext.fetch: multipart must be an object or FormData')
  }
  push(`--${boundary}--\r\n`)
  return { contentType: `multipart/form-data; boundary=${boundary}`, body: Buffer.concat(chunks) }
}

class APIResponse {
  #data
  #disposed = false
  #headersObj = null

  constructor(data) {
    this.#data = data
  }

  url() {
    return this.#data.url
  }

  status() {
    return this.#data.status
  }

  statusText() {
    return this.#data.statusText
  }

  ok() {
    return this.#data.status >= 200 && this.#data.status <= 299
  }

  headers() {
    if (!this.#headersObj) {
      const obj = {}
      for (const h of this.#data.headers) {
        const k = h.name.toLowerCase()
        obj[k] = k in obj ? `${obj[k]}${k === 'set-cookie' ? '\n' : ', '}${h.value}` : h.value
      }
      this.#headersObj = obj
    }
    return { ...this.#headersObj }
  }

  headersArray() {
    return this.#data.headers.map((h) => ({ name: h.name, value: h.value }))
  }

  async body() {
    if (this.#disposed) throw new Error('Response has been disposed')
    return this.#data.body
  }

  async text() {
    return (await this.body()).toString('utf8')
  }

  async json() {
    return JSON.parse(await this.text())
  }

  async dispose() {
    this.#disposed = true
    this.#data = { ...this.#data, body: Buffer.alloc(0) }
  }
}

class APIRequestContext {
  #sender
  #cookiesFn
  #isClosedFn
  #disposed = false

  constructor(sender, cookiesFn, isClosedFn) {
    this.#sender = sender
    this.#cookiesFn = cookiesFn
    this.#isClosedFn = isClosedFn
  }

  async fetch(url, options = {}) {
    checkOptions('apiRequestContext.fetch', options, API_REQ)
    if (this.#disposed || this.#isClosedFn?.()) throw new Error('apiRequestContext.fetch: target is closed or disposed')
    const bodyModes = [options.data, options.form, options.multipart].filter((x) => x !== undefined)
    if (bodyModes.length > 1) {
      throw new TypeError('apiRequestContext.fetch: pass at most one of data, form, or multipart')
    }
    const headers = {}
    if (options.headers) {
      for (const [k, v] of Object.entries(options.headers)) headers[k] = String(v)
    }
    let data
    if (options.data !== undefined) {
      if (Buffer.isBuffer(options.data) || typeof options.data === 'string') {
        data = options.data
        if (!hasHeaderCI(headers, 'content-type')) headers['content-type'] = 'application/octet-stream'
      } else {
        data = JSON.stringify(options.data)
        if (!hasHeaderCI(headers, 'content-type')) headers['content-type'] = 'application/json'
      }
    } else if (options.form !== undefined) {
      const sp = options.form instanceof URLSearchParams
        ? options.form
        : new URLSearchParams(Object.entries(options.form).map(([k, v]) => [k, String(v)]))
      data = sp.toString()
      if (!hasHeaderCI(headers, 'content-type')) headers['content-type'] = 'application/x-www-form-urlencoded'
    } else if (options.multipart !== undefined) {
      const mp = await encodeMultipartBody(options.multipart)
      data = mp.body
      if (!hasHeaderCI(headers, 'content-type')) headers['content-type'] = mp.contentType
    }
    const params = encodeQueryEntries(options.params)
    const raw = await this.#sender(String(url), {
      method: options.method,
      headers: Object.keys(headers).length ? headers : undefined,
      params,
      data,
      failOnStatusCode: options.failOnStatusCode,
      maxRedirects: options.maxRedirects,
      timeoutMs: options.timeout,
    })
    return new APIResponse(raw)
  }

  get(url, options) {
    return this.fetch(url, { ...options, method: 'GET' })
  }

  post(url, options) {
    return this.fetch(url, { ...options, method: 'POST' })
  }

  put(url, options) {
    return this.fetch(url, { ...options, method: 'PUT' })
  }

  patch(url, options) {
    return this.fetch(url, { ...options, method: 'PATCH' })
  }

  delete(url, options) {
    return this.fetch(url, { ...options, method: 'DELETE' })
  }

  head(url, options) {
    return this.fetch(url, { ...options, method: 'HEAD' })
  }

  async storageState(options) {
    checkOptions('apiRequestContext.storageState', options, { path: true })
    if (this.#disposed || this.#isClosedFn?.()) throw new Error('apiRequestContext.storageState: target is closed or disposed')
    const state = { cookies: await this.#cookiesFn(), origins: [] }
    if (options?.path) await savePrivate(options.path, JSON.stringify(state, null, 2))
    return state
  }

  async dispose() {
    this.#disposed = true
  }
}

function cookieFilterMatches(filter, value, isDomain = false) {
  if (filter instanceof RegExp) {
    filter.lastIndex = 0
    return filter.test(value)
  }
  if (!isDomain) return value === filter
  const f = filter.toLowerCase()
  const v = value.toLowerCase()
  return f.startsWith('.') ? v === f : v === f || v === `.${f}`
}

// browser.newContext() and context.newPage(), as in Playwright. Each context can hold multiple
// pages and popups sharing cookies, storage, init scripts, exposed functions and routes.
class BrowserContext {
  #browser
  #id
  #ensurePopup
  #closed = false
  #pages = new Set()
  #routes = []
  #routeStart = null
  #bindings = new Map()
  #bindingStarted = false
  #routeGetter
  #request

  constructor(browser, id, ensurePopup) {
    this.#browser = browser
    this.#id = id
    this.#ensurePopup = ensurePopup
    const ref = new WeakRef(this)
    this.#routeGetter = () => ref.deref()?.#routes ?? []
    contextRegistry.register(this, { browser, id }, this)
  }

  get request() {
    this.#request ??= new APIRequestContext(
      (url, opts) => this.#browser._contextApiFetch(this.#id, url, opts),
      () => this.#browser._contextCookies(this.#id, null),
      () => this.#closed,
    )
    return this.#request
  }

  browser() {
    return this.#browser
  }

  pages() {
    for (const p of this.#pages) {
      if (p.isClosed()) this.#pages.delete(p)
    }
    return [...this.#pages]
  }

  _adoptPage(page) {
    this.#pages.add(page)
    pageContext.set(page, new WeakRef(this))
    contextRouteGetters.set(page, this.#routeGetter)
    this.#ensurePopup(page)
  }

  _adoptPopup(popup) {
    if (this.#closed) return
    this._adoptPage(popup)
  }

  _removePage(page) {
    this.#pages.delete(page)
  }

  async newPage() {
    if (this.#closed) throw new Error('browserContext.newPage: the context is closed')
    const page = await this.#browser._newPageInContext(this.#id)
    if (this.#closed) {
      await page.close().catch(() => {})
      throw new Error('browserContext.newPage: the context is closed')
    }
    this._adoptPage(page)
    return page
  }

  // page.route for all pages and popups in this context; added before newPage(), it applies from the first request.
  async route(url, handler, options) {
    checkOptions('browserContext.route', options, NONE)
    if (this.#closed) throw new Error('browserContext.route: the context is closed')
    if (!this.#routeStart) {
      const ref = new WeakRef(this)
      this.#browser._setContextRouteHandler(this.#id, (native) => {
        const c = ref.deref()
        return dispatchRoutes(c ? c.#routes : [], [], native)
      })
      this.#routeStart = this.#browser._contextIntercept(this.#id)
      this.#routeStart.catch(() => {
        this.#routeStart = null
      })
    }
    await this.#routeStart
    this.#routes.push({ url, handler, matches: urlMatcher(url) })
  }

  async unroute(url, handler) {
    for (let i = this.#routes.length - 1; i >= 0; i--) {
      if (this.#routes[i].url === url && (!handler || this.#routes[i].handler === handler)) {
        this.#routes.splice(i, 1)
      }
    }
  }

  async addInitScript(script, arg) {
    if (this.#closed) throw new Error('browserContext.addInitScript: the context is closed')
    const source = await resolveInitScript('browserContext.addInitScript', script, arg)
    return this.#browser._contextAddInitScript(this.#id, source)
  }

  async exposeFunction(name, callback) {
    if (this.#closed) throw new Error('browserContext.exposeFunction: the context is closed')
    if (typeof name !== 'string' || !name) {
      throw new TypeError('browserContext.exposeFunction: name must be a non-empty string')
    }
    if (typeof callback !== 'function') {
      throw new TypeError('browserContext.exposeFunction: callback must be a function')
    }
    if (this.#bindings.has(name)) {
      throw new Error(`Function "${name}" has been already registered`)
    }
    if (!this.#bindingStarted) {
      this.#bindingStarted = true
      const browser = this.#browser
      const ref = new WeakRef(this)
      browser._contextOnBinding(this.#id, (ev) => {
        const c = ref.deref()
        if (!c) return
        return handleBindingEvent(
          (id, encoded) => browser._contextResolveBinding(id, encoded),
          (id, msg) => browser._contextRejectBinding(id, msg),
          c.#bindings,
          ev,
        )
      })
    }
    this.#bindings.set(name, callback)
    try {
      await this.#browser._contextExposeBinding(this.#id, name)
    } catch (err) {
      if (this.#bindings.get(name) === callback) this.#bindings.delete(name)
      throw err
    }
  }

  async storageState(options) {
    checkOptions('browserContext.storageState', options, { path: true, indexedDB: true })
    if (this.#closed) throw new Error('browserContext.storageState: the context is closed')
    const indexedDB = options?.indexedDB ?? false
    const pages = this.pages()
    let state
    if (!pages.length) {
      state = { cookies: await this.#browser._contextCookies(this.#id, null), origins: [] }
    } else {
      const states = await Promise.all(pages.map((p) => p.storageState({ indexedDB })))
      const byOrigin = new Map()
      for (const s of states) {
        for (const o of s.origins) byOrigin.set(o.origin, o)
      }
      state = { cookies: states[0].cookies, origins: [...byOrigin.values()] }
    }
    if (options?.path) await savePrivate(options.path, JSON.stringify(state, null, 2))
    return state
  }

  async cookies(urls) {
    if (this.#closed) throw new Error('browserContext.cookies: the context is closed')
    let urlList = null
    if (urls !== undefined) {
      urlList = typeof urls === 'string' ? [urls] : Array.isArray(urls) ? urls : null
      if (!urlList || urlList.some((u) => typeof u !== 'string')) {
        throw new TypeError('browserContext.cookies: urls must be a string or string[]')
      }
    }
    return this.#browser._contextCookies(this.#id, urlList)
  }

  async addCookies(cookies) {
    if (!Array.isArray(cookies)) throw new TypeError('browserContext.addCookies: cookies must be an array')
    if (this.#closed) throw new Error('browserContext.addCookies: the context is closed')
    const normalized = cookies.map((c) => {
      checkOptions('browserContext.addCookies cookie', c, {
        name: true,
        value: true,
        url: true,
        domain: true,
        path: true,
        expires: true,
        httpOnly: true,
        secure: true,
        sameSite: ['Strict', 'Lax', 'None'],
        partitionKey: true,
        _crHasCrossSiteAncestor: true,
      })
      if ((c.domain == null) === (c.url == null)) {
        throw new TypeError('browserContext.addCookies: each cookie needs url or domain, not both')
      }
      return {
        name: c.name,
        value: c.value,
        domain: c.domain ?? c.url ?? '',
        path: c.path ?? '/',
        expires: c.expires ?? -1,
        httpOnly: c.httpOnly ?? false,
        secure: c.secure ?? false,
        sameSite: c.sameSite ?? 'Lax',
        partitionKey: c.partitionKey,
        _crHasCrossSiteAncestor: c._crHasCrossSiteAncestor,
      }
    })
    return this.#browser._contextAddCookies(this.#id, normalized)
  }

  async clearCookies(options) {
    checkOptions('browserContext.clearCookies', options, { name: true, domain: true, path: true })
    if (this.#closed) throw new Error('browserContext.clearCookies: the context is closed')
    const hasFilter = options && (options.name !== undefined || options.domain !== undefined || options.path !== undefined)
    if (!hasFilter) return this.#browser._contextClearCookies(this.#id)
    for (const k of ['name', 'domain', 'path']) {
      if (options[k] !== undefined && typeof options[k] !== 'string' && !(options[k] instanceof RegExp)) {
        throw new TypeError(`browserContext.clearCookies: ${k} must be a string or RegExp`)
      }
    }
    const all = await this.#browser._contextCookies(this.#id, null)
    const keep = all.filter((c) => {
      const matchName = options.name === undefined || cookieFilterMatches(options.name, c.name, false)
      const matchDomain = options.domain === undefined || cookieFilterMatches(options.domain, c.domain, true)
      const matchPath = options.path === undefined || cookieFilterMatches(options.path, c.path, false)
      return !(matchName && matchDomain && matchPath)
    })
    if (keep.length !== all.length) {
      await this.#browser._contextReplaceCookies(this.#id, keep)
    }
  }

  async newCDPSession(page) {
    if (this.#closed) throw new Error('browserContext.newCDPSession: the context is closed')
    const frameEntry = page && typeof page === 'object' ? frameIds.get(page) : null
    const targetPage = frameEntry ? frameEntry.page : page
    const frameId = frameEntry ? frameEntry.id : null
    if (!targetPage || pageContext.get(targetPage)?.deref() !== this) {
      if (targetPage && typeof targetPage._targetId === 'function') {
        throw new Error('browserContext.newCDPSession: page does not belong to this context')
      }
      throw new TypeError('browserContext.newCDPSession: argument must be a Page or Frame')
    }
    const id = await targetPage._newCdpSession(frameId)
    return new CDPSession(targetPage, id)
  }

  async close(options) {
    checkOptions('browserContext.close', options, NONE)
    if (this.#closed) return
    this.#closed = true
    contextRegistry.unregister(this)
    await Promise.allSettled([...this.#pages].map((p) => (p.isClosed() ? Promise.resolve() : p.close())))
    this.#pages.clear()
    await this.#browser._closeContext(this.#id)
  }
}

// page.waitForDownload() with Playwright's Download.
function installDownloads(Page) {
  Page.prototype.waitForDownload = async function (options) {
    checkOptions('page.waitForDownload', options, TIMEOUT)
    const d = await this._waitForDownload(options?.timeout)
    return {
      url: () => d.url,
      suggestedFilename: () => d.suggestedFilename,
      path: async () => d.path,
      failure: async () => null,
      saveAs: async (path) => {
        await fs.mkdir(dirname(path), { recursive: true })
        await fs.copyFile(d.path, path)
      },
    }
  }
}

const TAG_SCRIPT = { url: true, path: true, content: true, type: true }
const TAG_STYLE = { url: true, path: true, content: true }

async function resolveInitScript(method, script, arg) {
  if (typeof script === 'function') return toExpression(script, arg)
  if (typeof script === 'string') return script
  if (script && typeof script === 'object') {
    checkOptions(method, script, { path: true, content: true })
    if (script.path != null && script.content != null) {
      throw new TypeError(`${method}: specify either path or content, not both`)
    }
    if (script.path != null) return fs.readFile(script.path, 'utf8')
    if (script.content != null) return String(script.content)
  }
  throw new TypeError(`${method}: script must be a function, string, or { path, content }`)
}

async function handleBindingEvent(resolveFn, rejectFn, map, ev) {
  const cb = map?.get(ev.name)
  if (!cb) {
    await rejectFn(ev.id, `Function "${ev.name}" is not exposed`).catch(() => {})
    return
  }
  try {
    const args = JSON.parse(ev.argsJson)
    const value = await cb(...args)
    const encoded = value === undefined ? 'null' : (JSON.stringify(value) ?? 'null')
    await resolveFn(ev.id, encoded).catch(() => {})
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err)
    await rejectFn(ev.id, msg).catch(() => {})
  }
}

function installBindings(Page) {
  const pages = new WeakMap()
  const starts = new WeakSet()

  function subscribe(page) {
    const ref = new WeakRef(page)
    page._onBinding((ev) => {
      const p = ref.deref()
      if (!p) return
      return handleBindingEvent(
        (id, encoded) => p._resolveBinding(id, encoded),
        (id, msg) => p._rejectBinding(id, msg),
        pages.get(p),
        ev,
      )
    })
  }

  Page.prototype.addInitScript = async function (script, arg) {
    const source = await resolveInitScript('page.addInitScript', script, arg)
    return this._addInitScript(source)
  }

  Page.prototype.exposeFunction = async function (name, callback) {
    if (typeof name !== 'string' || !name) {
      throw new TypeError('page.exposeFunction: name must be a non-empty string')
    }
    if (typeof callback !== 'function') {
      throw new TypeError('page.exposeFunction: callback must be a function')
    }
    let map = pages.get(this)
    if (!map) {
      map = new Map()
      pages.set(this, map)
    }
    if (map.has(name)) {
      throw new Error(`Function "${name}" has been already registered`)
    }
    if (!starts.has(this)) {
      subscribe(this)
      starts.add(this)
    }
    map.set(name, callback)
    try {
      await this._exposeBinding(name)
    } catch (err) {
      if (map.get(name) === callback) map.delete(name)
      throw err
    }
  }

  Page.prototype.addScriptTag = async function (options) {
    checkOptions('page.addScriptTag', options, TAG_SCRIPT)
    const sources = ['url', 'path', 'content'].filter((k) => options?.[k] != null)
    if (sources.length !== 1) {
      throw new TypeError('page.addScriptTag: provide exactly one of url, path, or content')
    }
    const content = options.path != null ? await fs.readFile(options.path, 'utf8') : options.content
    return this._addScriptTag(options.url, content, options.type)
  }

  Page.prototype.addStyleTag = async function (options) {
    checkOptions('page.addStyleTag', options, TAG_STYLE)
    const sources = ['url', 'path', 'content'].filter((k) => options?.[k] != null)
    if (sources.length !== 1) {
      throw new TypeError('page.addStyleTag: provide exactly one of url, path, or content')
    }
    const content = options.path != null ? await fs.readFile(options.path, 'utf8') : options.content
    return this._addStyleTag(options.url, content)
  }

  const close = Page.prototype.close
  Page.prototype.close = async function (options) {
    pages.delete(this)
    return close.call(this, options)
  }
}

function installFrames(Page, applyFilter) {
  const caches = new WeakMap()
  const frameMap = (page) => {
    let m = caches.get(page)
    if (!m) {
      m = new Map()
      caches.set(page, m)
    }
    return m
  }

  class Frame {
    #page
    #id
    #lastInfo = { parentId: null, name: '', url: 'about:blank' }

    constructor(page, info) {
      this.#page = page
      this.#id = info.id
      this.#update(info)
      frameIds.set(this, { page, id: info.id })
    }

    #update(info) {
      this.#lastInfo = {
        parentId: info.parentId ?? null,
        name: info.name ?? '',
        url: info.url ?? 'about:blank',
      }
    }

    #info() {
      const cur = this.#page._frames().find((f) => f.id === this.#id)
      if (cur) this.#update(cur)
      return cur
    }

    #root() {
      return this.#page._frameById(this.#id)
    }

    page() {
      return this.#page
    }

    url() {
      this.#info()
      return this.#lastInfo.url
    }

    name() {
      this.#info()
      return this.#lastInfo.name
    }

    isDetached() {
      return this.#page.isClosed() || this.#info() == null
    }

    parentFrame() {
      const info = this.#info()
      const pid = info?.parentId ?? this.#lastInfo.parentId
      if (!pid) return null
      return getFrameById(this.#page, pid)
    }

    childFrames() {
      return this.#page
        ._frames()
        .filter((f) => f.parentId === this.#id)
        .map((info) => getFrame(this.#page, info))
    }

    async goto(url, options) {
      checkOptions('frame.goto', options, NAV_OPTIONS)
      return this.#page._frameGoto(this.#id, url, options)
    }

    async evaluate(pageFunction, arg) {
      return this.#page._frameEvaluate(this.#id, toExpression(pageFunction, arg))
    }

    async title() {
      return this.#page._frameEvaluate(this.#id, 'document.title')
    }

    async content() {
      return this.#page._frameEvaluate(
        this.#id,
        "document.documentElement ? document.documentElement.outerHTML : ''",
      )
    }

    locator(selector, options) {
      let loc = this.#root().locator(selector)
      if (options != null) loc = applyFilter(loc, options, 'frame.locator')
      return loc
    }

    getByText(text, options) {
      checkOptions('frame.getByText', options, EXACT)
      return this.#root().getByText(text, options)
    }

    getByRole(role, options) {
      return this.#root().getByRole(role, options)
    }

    getByLabel(text, options) {
      checkOptions('frame.getByLabel', options, EXACT)
      return this.#root().getByLabel(text, options)
    }

    getByPlaceholder(text, options) {
      checkOptions('frame.getByPlaceholder', options, EXACT)
      return this.#root().getByPlaceholder(text, options)
    }

    getByAltText(text, options) {
      checkOptions('frame.getByAltText', options, EXACT)
      return this.#root().getByAltText(text, options)
    }

    getByTitle(text, options) {
      checkOptions('frame.getByTitle', options, EXACT)
      return this.#root().getByTitle(text, options)
    }

    getByTestId(testId) {
      return this.#root().getByTestId(testId)
    }

    frameLocator(selector) {
      return this.#root().frameLocator(selector)
    }
  }

  function getFrame(page, info) {
    const cache = frameMap(page)
    let f = cache.get(info.id)
    if (!f) {
      f = new Frame(page, info)
      cache.set(info.id, f)
    }
    return f
  }

  function getFrameById(page, id) {
    const info = page._frames().find((f) => f.id === id)
    if (info) return getFrame(page, info)
    return caches.get(page)?.get(id) ?? null
  }

  Page.prototype.frames = function () {
    return this._frames().map((info) => getFrame(this, info))
  }

  Page.prototype.mainFrame = function () {
    const id = this._mainFrameId()
    return (
      getFrameById(this, id) ??
      getFrame(this, { id, parentId: null, name: '', url: this.url() })
    )
  }

  Page.prototype.frame = function (frameSelector) {
    if (typeof frameSelector === 'string') {
      return this.frames().find((f) => f.name() === frameSelector) ?? null
    }
    if (frameSelector && typeof frameSelector === 'object') {
      checkOptions('page.frame', frameSelector, { name: true, url: true })
      if ((frameSelector.name == null) === (frameSelector.url == null)) {
        throw new TypeError('page.frame: provide either name or url, not both')
      }
      if (frameSelector.name != null) {
        return this.frames().find((f) => f.name() === frameSelector.name) ?? null
      }
      const matches = urlMatcher(frameSelector.url)
      return this.frames().find((f) => matches(f.url())) ?? null
    }
    throw new TypeError('page.frame: frameSelector must be a string or { name | url }')
  }
}

// Compiles only, nothing runs. Unlike `new Function`, this also works under
// `--disallow-code-generation-from-strings`.
function parses(source) {
  try {
    new Script(`(${source})`)
    return true
  } catch {
    return false
  }
}

function normalizeSelectValues(values) {
  if (values == null) return []
  const list = Array.isArray(values) ? values : [values]
  return list.map((item) => {
    if (typeof item === 'string') return { value: item }
    if (item && typeof item === 'object') {
      checkOptions('selectOption', item, { value: true, label: true, index: true })
      const count = (item.value !== undefined) + (item.label !== undefined) + (item.index !== undefined)
      if (count === 1) return item
    }
    throw new TypeError('selectOption: each entry must be a string or { value | label | index }')
  })
}

function applyFiles(target, pathsMethod, payloadsMethod, files) {
  if (files == null) throw new TypeError('setInputFiles: files must not be null or undefined')
  const list = Array.isArray(files) ? files : [files]
  if (list.length === 0) return target[pathsMethod]([])
  if (list.every((f) => typeof f === 'string')) return target[pathsMethod](list)
  const payloads = list.map((f) => {
    if (!f || typeof f !== 'object') {
      throw new TypeError('setInputFiles: files must be path strings or { name, mimeType, buffer } objects')
    }
    checkOptions('setInputFiles file', f, { name: true, mimeType: true, buffer: true })
    if (typeof f.name !== 'string' || typeof f.mimeType !== 'string' || !Buffer.isBuffer(f.buffer)) {
      throw new TypeError('setInputFiles: file payload needs string name, string mimeType, and Buffer buffer')
    }
    return { name: f.name, mimeType: f.mimeType, buffer: f.buffer }
  })
  return target[payloadsMethod](payloads)
}

try {
  const native = require('./index.js')
  const evaluate = native.Page.prototype.evaluate
  native.Page.prototype.evaluate = async function (pageFunction, arg) {
    return evaluate.call(this, toExpression(pageFunction, arg))
  }
  // As in Playwright, saved storage can live in a JSON file.
  async function normalizeContextOptions(method, options) {
    checkOptions(method, options, PAGE)
    if (typeof options?.storageState === 'string') {
      return { ...options, storageState: JSON.parse(await fs.readFile(options.storageState, 'utf8')) }
    }
    return options
  }
  const newPage = native.Browser.prototype.newPage
  native.Browser.prototype.newPage = async function (options) {
    const opts = await normalizeContextOptions('browser.newPage', options)
    const page = await newPage.call(this, opts)
    trackTarget(page)
    return page
  }
  const storageState = native.Page.prototype.storageState
  native.Page.prototype.storageState = async function (options) {
    checkOptions('page.storageState', options, { path: true, indexedDB: true })
    const state = await storageState.call(this, options?.indexedDB)
    if (options?.path) await savePrivate(options.path, JSON.stringify(state, null, 2))
    return state
  }
  const pageRequests = new WeakMap()
  Object.defineProperty(native.Page.prototype, 'request', {
    configurable: true,
    get() {
      const ctx = pageContext.get(this)?.deref()
      if (ctx) return ctx.request
      let req = pageRequests.get(this)
      if (!req) {
        req = new APIRequestContext(
          (url, opts) => this._apiFetch(url, opts),
          async () => (await storageState.call(this, false)).cookies,
          () => this.isClosed(),
        )
        pageRequests.set(this, req)
      }
      return req
    },
  })
  // Playwright's locator.evaluate(fn, arg) calls fn(element, arg).
  const evaluateOn = native.Locator.prototype.evaluate
  native.Locator.prototype.evaluate = async function (pageFunction, arg, options) {
    checkOptions('locator.evaluate', options, TIMEOUT)
    const source = typeof pageFunction === 'function' ? functionSource(pageFunction) : String(pageFunction)
    return evaluateOn.call(options?.timeout != null ? this._withTimeout(options.timeout) : this, source, arg)
  }
  // As Playwright's page.pageErrors(): Error objects.
  const pageErrors = native.Page.prototype.pageErrors
  native.Page.prototype.pageErrors = async function () {
    return (await pageErrors.call(this)).map(toError)
  }
  for (const [Class, label, allowed] of [
    [native.Page, 'page', { fullPage: true, clip: true, ...SHOT }],
    [native.Locator, 'locator', SHOT],
  ]) {
    const screenshot = Class.prototype.screenshot
    Class.prototype.screenshot = async function (options) {
      checkOptions(`${label}.screenshot`, options, allowed)
      if (options?.clip != null) {
        checkOptions(`${label}.screenshot clip`, options.clip, { x: true, y: true, width: true, height: true })
      }
      // As Playwright: without a type, the path's extension picks it.
      let type = options?.type
      if (!type && options?.path) {
        const ext = /\.([^./\\]+)$/.exec(options.path)?.[1]?.toLowerCase()
        type = ext === 'png' ? 'png' : ext === 'jpg' || ext === 'jpeg' ? 'jpeg' : undefined
        if (!type) throw new TypeError(`${label}.screenshot: ${options.path}: only .png, .jpg and .jpeg are supported`)
      }
      const image = await screenshot.call(this, type ? { ...options, type } : options)
      if (options?.path) await save(options.path, image)
      return image
    }
  }
  // Playwright's lengths are numbers of pixels or strings with a unit; the engine takes strings.
  const length = (v) => (typeof v === 'number' ? String(v) : v)
  const pdf = native.Page.prototype.pdf
  native.Page.prototype.pdf = async function (options) {
    checkOptions('page.pdf', options, PDF)
    if (options?.margin != null) {
      checkOptions('page.pdf margin', options.margin, { top: true, right: true, bottom: true, left: true })
    }
    const margin = options?.margin && Object.fromEntries(Object.entries(options.margin).map(([k, v]) => [k, length(v)]))
    const bytes = await pdf.call(this, options && { ...options, width: length(options.width), height: length(options.height), margin })
    if (options?.path) await save(options.path, bytes)
    return bytes
  }
function applyFilter(loc, options, label) {
  checkOptions(label, options, FILTER)
  if (options.hasText !== undefined) {
    if (options.hasText instanceof RegExp) loc = loc._hasTextRegex(options.hasText.source, options.hasText.flags)
    else if (typeof options.hasText === 'string') loc = loc._hasText(options.hasText)
    else throw new TypeError(`${label}: hasText must be a string or RegExp`)
  }
  if (options.hasNotText !== undefined) {
    if (options.hasNotText instanceof RegExp) loc = loc._hasNotTextRegex(options.hasNotText.source, options.hasNotText.flags)
    else if (typeof options.hasNotText === 'string') loc = loc._hasNotText(options.hasNotText)
    else throw new TypeError(`${label}: hasNotText must be a string or RegExp`)
  }
  if (options.has !== undefined) {
    if (!options.has || typeof options.has._frames !== 'function' || typeof options.has._query !== 'function') {
      throw new TypeError(`${label}: has must be a Locator`)
    }
    loc = loc._has(options.has._frames(), options.has._query())
  }
  if (options.hasNot !== undefined) {
    if (!options.hasNot || typeof options.hasNot._frames !== 'function' || typeof options.hasNot._query !== 'function') {
      throw new TypeError(`${label}: hasNot must be a Locator`)
    }
    loc = loc._hasNot(options.hasNot._frames(), options.hasNot._query())
  }
  return loc
}

  const builders = [
    ['getByText', 1, EXACT],
    ['getByLabel', 1, EXACT],
    ['getByPlaceholder', 1, EXACT],
    ['getByAltText', 1, EXACT],
    ['getByTitle', 1, EXACT],
  ]
  native.Page.prototype.emulateMedia = async function (options) {
    checkOptions('page.emulateMedia', options, {
      media: ['screen', 'print'],
      colorScheme: ['light', 'dark', 'no-preference'],
    })
    const has = (k) => options != null && Object.hasOwn(options, k) && options[k] !== undefined
    const media = has('media') ? (options.media ?? '') : undefined
    const scheme = has('colorScheme') ? (options.colorScheme ?? '') : undefined
    return this._emulateMedia(media, scheme)
  }
  // The native method takes the state itself, not Playwright's options object.
  const waitFor = native.Locator.prototype.waitFor
  native.Locator.prototype.waitFor = function (options) {
    return waitFor.call(this, options?.state)
  }
  const press = native.Locator.prototype.press
  native.Locator.prototype.press = function (key, options) {
    return press.call(this, key, options?.delay)
  }
  const pressSeq = native.Locator.prototype.pressSequentially
  native.Locator.prototype.pressSequentially = function (text, options) {
    return pressSeq.call(this, text, options?.delay)
  }
  const clear = native.Locator.prototype.clear
  native.Locator.prototype.clear = function (options) {
    return clear.call(this, options?.force)
  }
  const selectOption = native.Locator.prototype.selectOption
  native.Locator.prototype.selectOption = function (values, options) {
    return selectOption.call(this, normalizeSelectValues(values), options?.force)
  }
  native.Locator.prototype.setInputFiles = function (files) {
    return applyFiles(this, '_setInputPaths', '_setInputPayloads', files)
  }
  native.Locator.prototype.dragTo = function (target, options) {
    if (!target || typeof target._frames !== 'function' || typeof target._query !== 'function') {
      throw new TypeError('locator.dragTo: target must be a Locator')
    }
    return this._dragTo(target._frames(), target._query(), options?.force)
  }
  for (const [Class, label] of [
    [native.Page, 'page'],
    [native.FrameLocator, 'frameLocator'],
    [native.Locator, 'locator'],
  ]) {
    const origLocator = Class.prototype.locator
    Class.prototype.locator = function (selector, options) {
      let loc = origLocator.call(this, selector)
      if (options != null) loc = applyFilter(loc, options, `${label}.locator`)
      return loc
    }
    const origGetByRole = Class.prototype.getByRole
    const origGetByRoleRe = Class.prototype._getByRoleRegex
    Class.prototype.getByRole = function (role, options) {
      checkOptions(`${label}.getByRole`, options, ROLE)
      if (options?.name instanceof RegExp) {
        const { name, ...rest } = options
        return origGetByRoleRe.call(this, role, name.source, name.flags, rest)
      }
      if (options?.name != null && typeof options.name !== 'string') {
        throw new TypeError(`${label}.getByRole: name must be a string or RegExp`)
      }
      return origGetByRole.call(this, role, options)
    }
  }
  native.Locator.prototype.filter = function (options) {
    if (!options || typeof options !== 'object') {
      throw new TypeError('locator.filter: options must be an object')
    }
    return applyFilter(this, options, 'locator.filter')
  }
  native.Locator.prototype.and = function (locator) {
    if (!locator || typeof locator._frames !== 'function' || typeof locator._query !== 'function') {
      throw new TypeError('locator.and: argument must be a Locator')
    }
    return this._and(locator._frames(), locator._query())
  }
  native.Locator.prototype.or = function (locator) {
    if (!locator || typeof locator._frames !== 'function' || typeof locator._query !== 'function') {
      throw new TypeError('locator.or: argument must be a Locator')
    }
    return this._or(locator._frames(), locator._query())
  }
  native.Locator.prototype.all = async function () {
    const n = await this.count()
    return Array.from({ length: n }, (_, i) => this.nth(i))
  }
  guard(native.Page, 'page', true, builders)
  guard(native.FrameLocator, 'frameLocator', true, builders)
  guard(native.Locator, 'locator', true, builders)
  native.Page.prototype.waitForLoadState = async function (state, options) {
    if (state != null && !['load', 'domcontentloaded', 'networkidle'].includes(state)) {
      throw new TypeError(`page.waitForLoadState: state ${JSON.stringify(state)} is not supported`)
    }
    checkOptions('page.waitForLoadState', options, TIMEOUT)
    return this._waitForLoadState(state ?? 'load', options?.timeout)
  }
  native.Page.prototype.waitForURL = async function (url, options) {
    checkOptions('page.waitForURL', options, {
      waitUntil: ['load', 'domcontentloaded', 'networkidle', 'commit'],
      timeout: true,
    })
    let target = url
    if (typeof target === 'string') {
      const baseUrl = this._baseUrl()
      if (baseUrl != null && !/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(target)) {
        if (target.startsWith('/')) {
          target = new URL(baseUrl).origin + target
        } else if (!target.startsWith('*') && !target.includes('{')) {
          target = new URL(target, baseUrl).href
        }
      }
    } else if (!(target instanceof RegExp) && typeof target !== 'function') {
      throw new TypeError('page.waitForURL: url must be a string, RegExp or function')
    }
    const matches = urlMatcher(target)
    const waitUntil = options?.waitUntil ?? 'load'
    const rawTimeout = options?.timeout
    if (rawTimeout != null && (typeof rawTimeout !== 'number' || rawTimeout < 0)) {
      throw new TypeError('page.waitForURL: timeout must be a non-negative number')
    }
    const navTimeout = rawTimeout ?? this._navigationTimeout()
    const unlimited = navTimeout === 0 || navTimeout >= 365 * 24 * 3600 * 1000
    const deadline = unlimited ? Infinity : Date.now() + navTimeout
    while (true) {
      const current = this.url()
      if (matches(current)) {
        if (waitUntil === 'commit') return
        const remaining = unlimited ? 0 : Math.max(1, deadline - Date.now())
        return this._waitForLoadState(waitUntil, remaining)
      }
      if (!unlimited && Date.now() >= deadline) {
        throw new Error(`page.waitForURL: timed out waiting for URL (current: ${current})`)
      }
      const remaining = unlimited ? 0 : Math.max(1, deadline - Date.now())
      await this._waitForUrlChange(current, remaining)
    }
  }
  native.Page.prototype.waitForFunction = async function (pageFunction, arg, options) {
    checkOptions('page.waitForFunction', options, { polling: true, timeout: true })
    let pollingMs
    if (options?.polling !== undefined && options.polling !== 'raf') {
      if (typeof options.polling !== 'number' || !Number.isFinite(options.polling) || options.polling <= 0) {
        throw new TypeError("page.waitForFunction: polling must be 'raf' or a positive number")
      }
      pollingMs = options.polling
    }
    return this._waitForFunction(toExpression(pageFunction, arg), pollingMs, options?.timeout)
  }
  guard(native.Page, 'page', false, [
    ['goto', 1, NAV_OPTIONS],
    ['reload', 0, NAV_OPTIONS],
    ['goBack', 0, NAV_OPTIONS],
    ['goForward', 0, NAV_OPTIONS],
    ['setContent', 1, NAV_OPTIONS],
    ['click', 1, { ...CLICK, strict: true }, 'click'],
    ['dblclick', 1, { ...DBLCLICK, strict: true }, 'dblclick'],
    ['hover', 1, { ...HOVER, strict: true }, 'hover'],
    ['focus', 1, STRICT_TIMEOUT, 'focus'],
    ['fill', 2, STRICT_TIMEOUT, 'fill'],
    ['press', 2, { ...PRESS, strict: true }, 'press'],
    ['type', 2, { ...PRESS, strict: true }, 'pressSequentially'],
    ['check', 1, { ...CHECK, strict: true }, 'check'],
    ['uncheck', 1, { ...CHECK, strict: true }, 'uncheck'],
    ['setChecked', 2, { ...CHECK, strict: true }, 'setChecked'],
    ['selectOption', 2, { ...FORCE_TIMEOUT, strict: true }, 'selectOption'],
    ['setInputFiles', 2, STRICT_TIMEOUT, 'setInputFiles'],
    ['tap', 1, { ...HOVER, strict: true }, 'tap'],
    ['waitForSelector', 1, { ...STATE, ...STRICT_TIMEOUT }, 'waitFor'],
    ['textContent', 1, STRICT_TIMEOUT, 'textContent'],
    ['innerText', 1, STRICT_TIMEOUT, 'innerText'],
    ['innerHTML', 1, STRICT_TIMEOUT, 'innerHTML'],
    ['inputValue', 1, STRICT_TIMEOUT, 'inputValue'],
    ['getAttribute', 2, STRICT_TIMEOUT, 'getAttribute'],
    ['isVisible', 1, STRICT_TIMEOUT, 'isVisible'],
    ['isHidden', 1, STRICT_TIMEOUT, 'isHidden'],
    ['isEnabled', 1, STRICT_TIMEOUT, 'isEnabled'],
    ['isDisabled', 1, STRICT_TIMEOUT, 'isDisabled'],
    ['isChecked', 1, STRICT_TIMEOUT, 'isChecked'],
    ['isEditable', 1, STRICT_TIMEOUT, 'isEditable'],
  ])
  guard(native.Locator, 'locator', false, [
    ['click', 0, CLICK],
    ['dblclick', 0, DBLCLICK],
    ['hover', 0, HOVER],
    ['focus', 0, TIMEOUT],
    ['blur', 0, TIMEOUT],
    ['scrollIntoViewIfNeeded', 0, TIMEOUT],
    ['dragTo', 1, FORCE_TIMEOUT],
    ['press', 1, PRESS],
    ['pressSequentially', 1, PRESS],
    ['clear', 0, FORCE_TIMEOUT],
    ['check', 0, CHECK],
    ['uncheck', 0, CHECK],
    ['setChecked', 1, CHECK],
    ['selectOption', 1, FORCE_TIMEOUT],
    ['setInputFiles', 1, TIMEOUT],
    ['tap', 0, HOVER],
    ['waitFor', 0, { ...STATE, ...TIMEOUT }],
    ['textContent', 0, TIMEOUT],
    ['innerText', 0, TIMEOUT],
    ['innerHTML', 0, TIMEOUT],
    ['inputValue', 0, TIMEOUT],
    ['getAttribute', 1, TIMEOUT],
    ['count', 0, NONE],
    ['all', 0, NONE],
    ['allTextContents', 0, NONE],
    ['allInnerTexts', 0, NONE],
    ['isVisible', 0, TIMEOUT],
    ['isHidden', 0, TIMEOUT],
    ['isEnabled', 0, TIMEOUT],
    ['isDisabled', 0, TIMEOUT],
    ['isChecked', 0, TIMEOUT],
    ['isEditable', 0, TIMEOUT],
    ['boundingBox', 0, TIMEOUT],
  ])
  native.Locator.prototype.type = native.Locator.prototype.pressSequentially
  native.Page.prototype.dragAndDrop = async function (source, target, options) {
    checkOptions('page.dragAndDrop', options, { ...FORCE_TIMEOUT, strict: true })
    const strict = options?.strict ?? false
    const forwarded = options && Object.hasOwn(options, 'strict')
      ? (({ strict: _, ...rest }) => rest)(options)
      : options
    return this.locator(source)._strict(strict).dragTo(this.locator(target)._strict(strict), forwarded)
  }
  const devices = new WeakMap()
  const deviceState = (page) => {
    let d = devices.get(page)
    if (!d) {
      d = {
        mouse: {
          move: async (x, y, options) => {
            checkOptions('mouse.move', options, MOUSE_MOVE)
            return page._mouseMove(x, y, options?.steps)
          },
          down: async (options) => {
            checkOptions('mouse.down', options, MOUSE_BUTTON)
            return page._mouseDown(options?.button, options?.clickCount)
          },
          up: async (options) => {
            checkOptions('mouse.up', options, MOUSE_BUTTON)
            return page._mouseUp(options?.button, options?.clickCount)
          },
          click: async (x, y, options) => {
            checkOptions('mouse.click', options, MOUSE_CLICK)
            return page._mouseClick(x, y, options?.button, options?.clickCount, options?.delay)
          },
          dblclick: async (x, y, options) => {
            checkOptions('mouse.dblclick', options, MOUSE_DBLCLICK)
            return page._mouseDblclick(x, y, options?.button, options?.delay)
          },
          wheel: async (deltaX, deltaY) => page._mouseWheel(deltaX, deltaY),
        },
        keyboard: {
          down: async (key) => page._keyboardDown(key),
          up: async (key) => page._keyboardUp(key),
          press: async (key, options) => {
            checkOptions('keyboard.press', options, { delay: true })
            return page._keyboardPress(key, options?.delay)
          },
          type: async (text, options) => {
            checkOptions('keyboard.type', options, { delay: true })
            return page._keyboardType(text, options?.delay)
          },
          insertText: async (text) => page._keyboardInsertText(text),
        },
        touchscreen: {
          tap: async (x, y) => page._touchscreenTap(x, y),
        },
      }
      devices.set(page, d)
    }
    return d
  }
  for (const name of ['mouse', 'keyboard', 'touchscreen']) {
    Object.defineProperty(native.Page.prototype, name, {
      get() {
        return deviceState(this)[name]
      },
      enumerable: true,
      configurable: true,
    })
  }
  const ensurePopup = installEvents(native.Page)
  native.Browser.prototype.newContext = async function (options) {
    const opts = await normalizeContextOptions('browser.newContext', options)
    const id = await this._newContext(opts)
    return new BrowserContext(this, id, ensurePopup)
  }
  installRoutes(native.Page)
  installDownloads(native.Page)
  installBindings(native.Page)
  installFrames(native.Page, applyFilter)
  module.exports = native
  // Chromium.launch is read-only on the native class, so `chromium` is a checked stand-in.
  module.exports.chromium = {
    launch: async (options) => {
      checkOptions('chromium.launch', options, LAUNCH)
      return native.Chromium.launch(options)
    },
    connectOverCDP: async (endpointURL, options) => {
      checkOptions('chromium.connectOverCDP', options, NONE)
      return native.Chromium.connectOverCDP(endpointURL)
    },
  }
  module.exports.default = module.exports
} catch (err) {
  // Keep why: no prebuild for this platform, a missing optional dependency, or a checkout that
  // was never built.
  const error = new Error(
    `fluxwright: the native addon failed to load: ${err.message}. Prebuilds cover Windows x64, ` +
      'macOS x64 and arm64, and Linux x64 (glibc); elsewhere, build it from a source checkout ' +
      '(bindings/node: npm run build).',
    { cause: err },
  )
  module.exports = {
    chromium: {
      launch: async () => {
        throw error
      },
      connectOverCDP: async () => {
        throw error
      },
    },
  }
  module.exports.default = module.exports
}
