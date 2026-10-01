'use strict'

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
    return (await pageErrors.call(this)).map((e) => {
      const err = new Error(e.message)
      if (e.name) err.name = e.name
      err.stack = e.stack
      return err
    })
  }
  for (const Class of [native.Page, native.Locator]) {
    const screenshot = Class.prototype.screenshot
    Class.prototype.screenshot = async function (options) {
      const png = await screenshot.call(this, options)
      if (options?.path) await save(options.path, png)
      return png
    }
  }
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
