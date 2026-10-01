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

// page.on / off / once for 'console' and 'pageerror', as in Playwright.
function installEvents(Page) {
  const emitters = new WeakMap()
  const events = ['console', 'pageerror']
  const emitter = (page) => {
    let em = emitters.get(page)
    if (!em) {
      em = new EventEmitter()
      emitters.set(page, em)
      page._onLog((e) => (e.event === 'console' ? em.emit('console', e.message) : em.emit('pageerror', toError(e.error))))
    }
    return em
  }
  for (const method of ['on', 'once', 'addListener']) {
    Page.prototype[method] = function (event, handler) {
      if (!events.includes(event)) throw new Error(`page.${method}: '${event}' is not supported (${events.join(', ')})`)
      emitter(this)[method === 'addListener' ? 'on' : method](event, handler)
      return this
    }
  }
  for (const method of ['off', 'removeListener']) {
    Page.prototype[method] = function (event, handler) {
      emitters.get(this)?.off(event, handler)
      return this
    }
  }
}

// Playwright's URL patterns: a glob string (** any characters, * any but /, {a,b} either), a
// RegExp, or a function of the URL.
function urlMatcher(pattern) {
  if (typeof pattern === 'function') return (url) => pattern(new URL(url))
  if (pattern instanceof RegExp) return (url) => pattern.test(url)
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
  const request = (native) => ({
    url: () => native.url,
    method: () => native.method,
    headers: () => ({ ...native.headers }),
    postData: () => native.postData ?? null,
    resourceType: () => native.resourceType,
  })
  const dispatch = async (list, native) => {
    for (let i = list.length - 1; i >= 0; i--) {
      if (!list[i].matches(native.url)) continue
      let decided = null
      const route = {
        request: () => request(native),
        fulfill: async (o = {}) => {
          decided = 'handled'
          const headers = { ...o.headers }
          let body = o.body
          if (o.json !== undefined) {
            body = JSON.stringify(o.json)
            headers['content-type'] ??= 'application/json'
          }
          if (o.path) body = await fs.readFile(o.path)
          if (o.contentType) headers['content-type'] = o.contentType
          return native.fulfill({ status: o.status, headers, body })
        },
        continue: async (o = {}) => {
          decided = 'handled'
          return native.continue({ url: o.url, method: o.method, headers: o.headers, postData: o.postData })
        },
        abort: async (errorCode) => {
          decided = 'handled'
          return native.abort(errorCode)
        },
        fallback: async () => {
          decided = 'fallback'
        },
      }
      try {
        await list[i].handler(route, route.request())
      } catch (err) {
        console.error('[fluxwright] a route handler threw; the request continues:', err)
        if (!decided) await native.continue().catch(() => {})
        return
      }
      if (decided !== 'fallback') return
    }
    await native.continue().catch(() => {})
  }
  Page.prototype.route = async function (url, handler) {
    let list = routes.get(this)
    if (!list) {
      list = []
      routes.set(this, list)
      this._setRouteHandler((native) => dispatch(list, native))
      await this._intercept()
    }
    list.push({ url, handler, matches: urlMatcher(url) })
  }
  Page.prototype.unroute = async function (url, handler) {
    const list = routes.get(this) || []
    for (let i = list.length - 1; i >= 0; i--) {
      if (list[i].url === url && (!handler || list[i].handler === handler)) list.splice(i, 1)
    }
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
    if (options?.path) await save(options.path, JSON.stringify(state, null, 2))
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
  installEvents(native.Page)
  installRoutes(native.Page)
  installDownloads(native.Page)
  module.exports = native
  module.exports.chromium = native.Chromium
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
