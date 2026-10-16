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

// Playwright's Request, Response and Dialog over the native objects.
function toRequest(native, failure = null) {
  return {
    url: () => native.url,
    method: () => native.method,
    headers: () => ({ ...native.headers }),
    postData: () => native.postData ?? null,
    resourceType: () => native.resourceType,
    failure: () => (failure === null ? null : { errorText: failure }),
  }
}

function toResponse(native) {
  const request = toRequest(native.request)
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

function toDialog(native) {
  const dialog = {
    answered: false,
    type: () => native.type,
    message: () => native.message,
    defaultValue: () => native.defaultValue,
    accept: (promptText) => {
      dialog.answered = true
      return native.accept(promptText)
    },
    dismiss: () => {
      dialog.answered = true
      return native.dismiss()
    },
  }
  return dialog
}

// Every dialog listener runs, and their promises are awaited. A dialog none of them answered is
// dismissed, so the page never hangs on it.
async function answerDialog(em, dialog) {
  const pending = em.rawListeners('dialog').map(async (handler) => handler(dialog))
  for (const r of await Promise.allSettled(pending)) {
    if (r.status === 'rejected') console.error('[fluxwright] a dialog listener failed:', r.reason)
  }
  if (!dialog.answered) await dialog.dismiss().catch(() => {})
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
  const emitter = (page, event) => {
    let state = emitters.get(page)
    if (!state) {
      const em = new EventEmitter()
      em.setMaxListeners(0) // waitForResponse adds one per call
      state = { em, started: new Set() }
      emitters.set(page, state)
    }
    const { em, started } = state
    const source = sources[event]
    if (!started.has(source)) {
      started.add(source)
      if (source === 'log') {
        page._onLog((e) => (e.event === 'console' ? em.emit('console', e.message) : em.emit('pageerror', toError(e.error))))
      } else if (source === 'network') {
        page._onNetwork((e) =>
          em.emit(e.event, e.event === 'response' ? toResponse(e.response) : toRequest(e.request, e.failure ?? null)),
        )
      } else {
        page._onDialog((native) => answerDialog(em, toDialog(native)))
      }
    }
    return em
  }
  for (const method of ['on', 'once', 'addListener']) {
    Page.prototype[method] = function (event, handler) {
      if (!events.includes(event)) throw new Error(`page.${method}: '${event}' is not supported (${events.join(', ')})`)
      emitter(this, event)[method === 'addListener' ? 'on' : method](event, handler)
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
  // gets the Response or Request and may return a promise.
  for (const [method, event] of [
    ['waitForResponse', 'response'],
    ['waitForRequest', 'request'],
  ]) {
    Page.prototype[method] = function (urlOrPredicate, options) {
      const timeout = options?.timeout ?? 30000
      let matches = urlOrPredicate
      if (typeof urlOrPredicate !== 'function') {
        const url = urlMatcher(urlOrPredicate)
        matches = (r) => url(r.url())
      }
      return new Promise((resolve, reject) => {
        let timer = null
        const done = () => {
          this.off(event, listener)
          clearTimeout(timer)
        }
        const listener = async (r) => {
          try {
            if (!(await matches(r))) return
          } catch (err) {
            done()
            return reject(err)
          }
          done()
          resolve(r)
        }
        this.on(event, listener)
        if (timeout > 0) {
          timer = setTimeout(() => {
            done()
            reject(new Error(`page.${method}: timeout ${timeout}ms exceeded`))
          }, timeout)
        }
      })
    }
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
  const request = (native) => toRequest(native)
  const dispatch = async (list, native) => {
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
        // when the handler returns is waited for, not taken as no answer.
        let answer = null
        let fellBack = false
        const answerWith = (work) => {
          if (answer || fellBack) return Promise.reject(new Error('route is already handled'))
          answer = work()
          return answer
        }
        const route = {
          request: () => request(native),
          fulfill: (o = {}) =>
            answerWith(async () => {
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
            answerWith(() => native.continue({ url: o.url, method: o.method, headers: o.headers, postData: o.postData })),
          abort: (errorCode) => answerWith(() => native.abort(errorCode)),
          fallback: async () => {
            if (answer) throw new Error('route is already handled')
            fellBack = true
          },
        }
        try {
          await list[i].handler(route, route.request())
          if (fellBack && !answer) continue
          if (answer) await answer
        } catch (err) {
          console.error('[fluxwright] a route handler failed; the request continues:', err)
        }
        return
      }
    } finally {
      // No answer, or one that failed: let the request through rather than hang the page.
      // A request already answered refuses this, harmlessly.
      await native.continue().catch(() => {})
    }
  }
  // One interception start per page. Every route() call waits for it, so none returns before
  // requests are handed over, and a call adds its handler only once the start succeeded (a
  // failed start is tried again by the next call).
  const starts = new WeakMap()
  Page.prototype.route = async function (url, handler) {
    let list = routes.get(this)
    if (!list) {
      list = []
      routes.set(this, list)
    }
    let start = starts.get(this)
    if (!start) {
      this._setRouteHandler((native) => dispatch(list, native))
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
  async route(url, handler) {
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

  async cookies() {
    return (await this.storageState()).cookies
  }

  async close() {
    this.#closed = true
    if (this.#page) await this.#page.close()
  }
}

// page.waitForDownload() with Playwright's Download.
function installDownloads(Page) {
  Page.prototype.waitForDownload = async function (options) {
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
    if (typeof options?.storageState === 'string') {
      options = { ...options, storageState: JSON.parse(await fs.readFile(options.storageState, 'utf8')) }
    }
    return newPage.call(this, options)
  }
  const storageState = native.Page.prototype.storageState
  native.Page.prototype.storageState = async function (options) {
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
  for (const Class of [native.Page, native.Locator]) {
    const screenshot = Class.prototype.screenshot
    Class.prototype.screenshot = async function (options) {
      const png = await screenshot.call(this, options)
      if (options?.path) await save(options.path, png)
      return png
    }
  }
  native.Browser.prototype.newContext = async function (options) {
    return new BrowserContext(this, options)
  }
  installEvents(native.Page)
  installRoutes(native.Page)
  installDownloads(native.Page)
  module.exports = native
  module.exports.chromium = native.Chromium
  module.exports.BrowserContext = BrowserContext
  module.exports.default = module.exports
} catch (err) {
  module.exports = {
    chromium: {
      launch: async () => {
        throw new Error('build the native addon: npm run build')
      },
    },
  }
  module.exports.default = module.exports
}
