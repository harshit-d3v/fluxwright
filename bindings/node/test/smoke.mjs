// Smoke test of the published entry point: `npm test` after `npm run build`. Needs Chrome or
// chrome-headless-shell (see the README).
import assert from 'node:assert/strict'
import { once } from 'node:events'
import http from 'node:http'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import fluxwright from '../addon.js'

// Echoes the user agent and languages; /login sets a cookie.
const server = http.createServer((req, res) => {
  if (req.url === '/login') res.setHeader('Set-Cookie', 'sid=s3cret; Path=/')
  res.setHeader('Content-Type', 'text/html')
  res.end(`<title>${req.headers['user-agent']}|${req.headers['accept-language']}</title>`)
})
server.listen(0, '127.0.0.1')
await once(server, 'listening')
server.unref()
const base = `http://127.0.0.1:${server.address().port}`

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

  // Per-page emulation, with Playwright's option names.
  const emulated = await browser.newPage({
    userAgent: 'FluxSmoke/1',
    locale: 'fr-FR',
    timezoneId: 'Europe/Paris',
    viewport: { width: 600, height: 500 },
    deviceScaleFactor: 2,
    colorScheme: 'dark',
  })
  await emulated.goto(base)
  assert.match(await emulated.title(), /^FluxSmoke\/1\|fr-FR/)
  assert.equal(
    await emulated.evaluate(() =>
      [
        navigator.language,
        Intl.DateTimeFormat().resolvedOptions().timeZone,
        innerWidth,
        devicePixelRatio,
        matchMedia('(prefers-color-scheme: dark)').matches,
      ].join('|'),
    ),
    'fr-FR|Europe/Paris|600|2|true',
  )
  await emulated.close()
  await assert.rejects(browser.newPage({ colorScheme: 'purple' }), /colorScheme/)

  // Log in once, save to a file, start another page from it.
  const login = await browser.newPage()
  await login.goto(`${base}/login`)
  await login.evaluate(() => localStorage.setItem('token', 't1'))
  const file = join(tmpdir(), `fluxwright-state-${process.pid}.json`)
  const state = await login.storageState({ path: file })
  assert.ok(state.cookies.some((c) => c.name === 'sid' && c.value === 's3cret'))
  await login.close()
  const again = await browser.newPage({ storageState: file })
  await again.goto(base)
  assert.equal(await again.evaluate(() => `${document.cookie}|${localStorage.getItem('token')}`), 'sid=s3cret|t1')
  await again.close()

  // Left open on purpose: Node drops it at exit, outside the engine's runtime (0.2.1 crashed there).
  await browser.newPage()
} finally {
  await browser.close()
  server.close()
}
console.log('smoke ok')
