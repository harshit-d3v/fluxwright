// Smoke test of the published entry point: `npm test` after `npm run build`. Needs Chrome or
// chrome-headless-shell (see the README).
import assert from 'node:assert/strict'
import fluxwright from '../addon.js'

const browser = await fluxwright.chromium.launch({ maxBrowsers: 1 })
try {
  const page = await browser.newPage()
  await page.goto('data:text/html,<title>Hi</title><p>x</p>')

  assert.equal(await page.evaluate('document.title'), 'Hi')
  assert.equal(await page.evaluate(() => document.title), 'Hi')
  assert.equal(await page.evaluate(({ a, b }) => a + b, { a: 2, b: 3 }), 5)
  assert.equal(await page.evaluate(async (s) => s.toUpperCase(), 'ok'), 'OK')
  assert.deepEqual(await page.evaluate((xs) => xs.map((x) => x * 2), [1, 2]), [2, 4])
  await assert.rejects(
    page.evaluate(() => {
      throw new Error('boom')
    }),
    /boom/,
  )

  // Methods stringify as `name(x) { ... }`, which needs turning into a function expression.
  const methods = {
    add(x) {
      return x + 1
    },
    async twice(x) {
      return x * 2
    },
  }
  class Calc {
    triple(x) {
      return x * 3
    }
  }
  assert.equal(await page.evaluate(methods.add, 1), 2)
  assert.equal(await page.evaluate(methods.twice, 4), 8)
  assert.equal(await page.evaluate(new Calc().triple, 2), 6)
  await assert.rejects(page.evaluate(Math.max), /can't be sent to the page/)
  await page.close()
  // Left open on purpose: Node drops it at exit, outside the engine's runtime (0.2.1 crashed there).
  await browser.newPage()
} finally {
  await browser.close()
}
console.log('smoke ok')
