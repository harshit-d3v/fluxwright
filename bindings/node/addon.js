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
const VISIBLE = { state: ['visible'] }
const TIMEOUT = { timeout: true }
const SHOT = { path: true, type: ['png', 'jpeg'], quality: true, omitBackground: true }
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
const LAUNCH = { maxBrowsers: true, executablePath: true, headless: true, queueTimeout: true, memoryCeilingMb: true }
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
  storageState: true,
}
const FULFILL = { status: true, headers: true, contentType: true, body: true, json: true, path: true }
const CONTINUE = { url: true, method: true, headers: true, postData: true }

function checkOptions(method, options, allowed) {
  if (options == null) return
  if (typeof options !== 'object') throw new TypeError(`${method}: options must be an object`)
  for (const [key, value] of Object.entries(options)) {
    if (value == null) continue
    const ok = Object.hasOwn(allowed, key) && allowed[key]
    if (!ok) throw new TypeError(`${method}: option '${key}' is not supported (see MISSING.md)`)
    // NaN reaches napi as 0: an unset env var would make `maxBrowsers: 0` or no memory budget.
    if (Number.isNaN(value)) throw new TypeError(`${method}: option '${key}' is NaN`)
    if (Array.isArray(ok) && !ok.includes(value)) {
      const supported = ok.map((v) => JSON.stringify(v)).join(', ')
      throw new TypeError(`${method}: ${key} ${JSON.stringify(value)} is not supported, only ${supported} (see MISSING.md)`)
    }
  }
}

// Checks the options argument (at index `at`) of native methods first. Async ones reject rather
// than throw, as the native call would.
function guard(Class, label, sync, methods) {
  for (const [name, at, allowed] of methods) {
    const call = Class.prototype[name]
    Class.prototype[name] = sync
      ? function (...args) {
          checkOptions(`${label}.${name}`, args[at], allowed)
          return call.apply(this, args)
        }
      : async function (...args) {
          checkOptions(`${label}.${name}`, args[at], allowed)
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

// page.on / once / off as in Playwright, for console messages, page errors, network events and
// dialogs. Each source starts with its first listener.
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
  }
  const events = Object.keys(sources)
  // Pages whose close() was called: the native flag goes up only once napi runs the call.
  const closing = new WeakSet()
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
  // page.waitForResponse / waitForRequest: the first match from now on, within `timeout` ms
  // (default 30 s, 0 for none). A string or RegExp matches the URL as page.route does; a function
  // gets the Response or Request and may return a promise. Closing the page rejects it.
  for (const [method, event] of [
    ['waitForResponse', 'response'],
    ['waitForRequest', 'request'],
  ]) {
    Page.prototype[method] = function (urlOrPredicate, options) {
      return new Promise((resolve, reject) => {
        checkOptions(`page.${method}`, options, TIMEOUT)
        if (closing.has(this) || this.isClosed()) throw new Error(`page.${method}: the page is closed`)
        const timeout = options?.timeout ?? 30000
        let matches = urlOrPredicate
        if (typeof urlOrPredicate !== 'function') {
          const url = urlMatcher(urlOrPredicate)
          matches = (r) => url(r.url())
        }
        const { em, waits } = stateOf(this, event)
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
  }
  // As in Playwright, closing the page rejects its pending waits at once; calls still running on
  // the page reject on the native side.
  const close = Page.prototype.close
  Page.prototype.close = async function (options) {
    checkOptions('page.close', options, NONE)
    closing.add(this)
    for (const closed of emitters.get(this)?.waits ?? []) closed()
    return close.call(this)
  }
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
function installRoutes(Page) {
  const routes = new WeakMap()
  const dispatch = async (list, native) => {
    const request = toRequest(native)
    let answered = false
    try {
      for (let i = list.length - 1; i >= 0; i--) {
        // A URL function is user code: one that throws counts as no match.
        let matched = false
        try {
          matched = list[i].matches(native.url)
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
          fulfill: (o = {}) =>
            answerWith(async () => {
              checkOptions('route.fulfill', o, FULFILL)
              const headers = { ...o.headers }
              let body = o.body
              if (o.json !== undefined) {
                body = JSON.stringify(o.json)
                headers['content-type'] ??= 'application/json'
              }
              if (o.path) body = await fs.readFile(o.path)
              if (o.contentType) headers['content-type'] = o.contentType
              return native.fulfill({ status: o.status, headers, body })
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
          await list[i].handler(route, request)
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
    } finally {
      // No answer, or one that failed: let the request through rather than hang the page.
      if (!answered) await native.continue().catch(() => {})
    }
  }
  // The native side keeps the dispatcher as long as the page's lease. It reaches the route list
  // through a WeakRef, so handlers that use the page don't keep a forgotten page alive. Made apart
  // from route(), whose scope holds the page.
  const dispatcher = (page) => {
    const ref = new WeakRef(page)
    return (native) => dispatch(routes.get(ref.deref()) ?? [], native)
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

const ONE_PAGE =
  'a Fluxwright context holds one page, so jobs never share cookies: open another context, ' +
  'and pass it storageState() from this one to share a login'

// browser.newContext() and context.newPage(), as in Playwright. Each context holds one page, so
// one job's cookies and storage never reach another; a login is shared through storageState.
class BrowserContext {
  #browser
  #options
  #page = null
  #opening = false
  #closed = false
  #routes = []

  constructor(browser, options) {
    this.#browser = browser
    this.#options = options
  }

  browser() {
    return this.#browser
  }

  pages() {
    return this.#page && !this.#page.isClosed() ? [this.#page] : []
  }

  async newPage() {
    if (this.#closed) throw new Error('browserContext.newPage: the context is closed')
    if (this.#page || this.#opening) throw new Error(`browserContext.newPage: ${ONE_PAGE}`)
    this.#opening = true
    try {
      const page = await this.#browser.newPage(this.#options)
      try {
        for (const [url, handler] of this.#routes) await page.route(url, handler)
        // close() ran while the page was coming: it would otherwise stay open on a closed context.
        if (this.#closed) throw new Error('browserContext.newPage: the context is closed')
      } catch (err) {
        await page.close()
        throw err
      }
      this.#page = page
      return page
    } finally {
      this.#opening = false
    }
  }

  // page.route for this context's page; added before newPage(), it applies from the first request.
  async route(url, handler, options) {
    checkOptions('browserContext.route', options, NONE)
    this.#routes.push([url, handler])
    if (this.#page) await this.#page.route(url, handler)
  }

  async unroute(url, handler) {
    this.#routes = this.#routes.filter(([u, h]) => !(u === url && (!handler || h === handler)))
    if (this.#page) await this.#page.unroute(url, handler)
  }

  async storageState(options) {
    if (!this.#page) throw new Error('browserContext.storageState: call newPage() first')
    return this.#page.storageState(options)
  }

  async cookies(urls) {
    if (urls !== undefined) throw new TypeError('browserContext.cookies: filtering by URL is not supported (see MISSING.md)')
    return (await this.storageState()).cookies
  }

  async close(options) {
    checkOptions('browserContext.close', options, NONE)
    this.#closed = true
    if (this.#page) await this.#page.close()
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

try {
  const native = require('./index.js')
  const evaluate = native.Page.prototype.evaluate
  native.Page.prototype.evaluate = async function (pageFunction, arg) {
    return evaluate.call(this, toExpression(pageFunction, arg))
  }
  // As in Playwright, saved storage can live in a JSON file.
  const newPage = native.Browser.prototype.newPage
  native.Browser.prototype.newPage = async function (options) {
    checkOptions('browser.newPage', options, PAGE)
    if (typeof options?.storageState === 'string') {
      options = { ...options, storageState: JSON.parse(await fs.readFile(options.storageState, 'utf8')) }
    }
    return newPage.call(this, options)
  }
  const storageState = native.Page.prototype.storageState
  native.Page.prototype.storageState = async function (options) {
    checkOptions('page.storageState', options, { path: true })
    const state = await storageState.call(this)
    if (options?.path) await savePrivate(options.path, JSON.stringify(state, null, 2))
    return state
  }
  // Playwright's locator.evaluate(fn, arg) calls fn(element, arg).
  const evaluateOn = native.Locator.prototype.evaluate
  native.Locator.prototype.evaluate = async function (pageFunction, arg) {
    const source = typeof pageFunction === 'function' ? functionSource(pageFunction) : String(pageFunction)
    return evaluateOn.call(this, source, arg)
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
  const builders = [
    ['locator', 1, NONE],
    ['getByText', 1, EXACT],
    ['getByRole', 1, { name: true, exact: true }],
    ['getByLabel', 1, EXACT],
    ['getByPlaceholder', 1, EXACT],
  ]
  guard(native.Page, 'page', true, builders)
  guard(native.FrameLocator, 'frameLocator', true, builders)
  guard(native.Locator, 'locator', true, [...builders, ['filter', 0, { hasText: true }]])
  guard(native.Page, 'page', false, [
    ['goto', 1, { waitUntil: true }],
    ['click', 1, NONE],
    ['fill', 2, NONE],
    ['waitForSelector', 1, VISIBLE],
  ])
  guard(native.Locator, 'locator', false, [
    ['click', 0, NONE],
    ['fill', 1, NONE],
    ['waitFor', 0, VISIBLE],
    ['textContent', 0, NONE],
    ['boundingBox', 0, NONE],
  ])
  native.Browser.prototype.newContext = async function (options) {
    checkOptions('browser.newContext', options, PAGE)
    return new BrowserContext(this, options)
  }
  installEvents(native.Page)
  installRoutes(native.Page)
  installDownloads(native.Page)
  module.exports = native
  // Chromium.launch is read-only on the native class, so `chromium` is a checked stand-in.
  module.exports.chromium = {
    launch: async (options) => {
      checkOptions('chromium.launch', options, LAUNCH)
      return native.Chromium.launch(options)
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
    },
  }
  module.exports.default = module.exports
}
