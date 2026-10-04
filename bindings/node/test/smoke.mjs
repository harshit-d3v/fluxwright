// Smoke test of the published entry point: `npm test` after `npm run build`. Needs Chrome or
// chrome-headless-shell (see the README).
import assert from 'node:assert/strict'
import { once } from 'node:events'
import { chmodSync, statSync } from 'node:fs'
import { readFile } from 'node:fs/promises'
import http from 'node:http'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import fluxwright from '../addon.js'

// Echoes the user agent and languages; /login sets a cookie; /report is a download; /hdr echoes
// x-test; /api is the real API that routes stand in for.
const server = http.createServer((req, res) => {
  if (req.url === '/report') {
    res.setHeader('Content-Disposition', 'attachment; filename="report.csv"')
    return res.end('a,b\n1,2\n')
  }
  if (req.url === '/hdr') {
    res.setHeader('Content-Type', 'text/plain')
    return res.end(String(req.headers['x-test']))
  }
  if (req.url === '/api') {
    res.setHeader('Content-Type', 'application/json')
    return res.end('{"real":true}')
  }
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
  // A folder that does not exist yet, as with Playwright's `.auth/state.json`.
  const file = join(tmpdir(), `fluxwright-${process.pid}`, '.auth', 'state.json')
  const state = await login.storageState({ path: file })
  assert.ok(state.cookies.some((c) => c.name === 'sid' && c.value === 's3cret'))
  if (process.platform !== 'win32') {
    // Saved cookies are the owner's alone, also when the file already existed with a wider mode.
    assert.equal(statSync(file).mode & 0o077, 0)
    assert.equal(statSync(dirname(file)).mode & 0o077, 0)
    chmodSync(file, 0o644)
    await login.storageState({ path: file })
    assert.equal(statSync(file).mode & 0o077, 0)
  }
  await login.close()
  const again = await browser.newPage({ storageState: file })
  await again.goto(base)
  assert.equal(await again.evaluate(() => `${document.cookie}|${localStorage.getItem('token')}`), 'sid=s3cret|t1')
  await again.close()

  // Locators, element screenshots, boxes, evaluate on an element, console and page errors.
  const ui = await browser.newPage()
  await ui.goto(
    'data:text/html,<body style="margin:0"><label>Email <input id=e></label><input placeholder="Your city" id=c>' +
      '<b data-testid=total style="position:absolute;left:10px;top:50px;width:40px;height:20px;background:red">7</b>' +
      '<ul><li>Apple <button onclick="document.title=1">Buy</button></li><li>Pear <button onclick="document.title=2">Buy</button></li></ul>' +
      '<script>console.log("ready", 1); setTimeout(() => { throw new TypeError("late") }, 0)</script>',
  )
  await ui.getByLabel('email').fill('a@b.c')
  await ui.getByPlaceholder('city').fill('Pune')
  assert.equal(await ui.evaluate(() => [e.value, c.value].join('|')), 'a@b.c|Pune')
  assert.equal(await ui.getByTestId('total').textContent(), '7')
  assert.match(await ui.locator('li').last().textContent(), /^Pear/)
  await ui.getByRole('listitem').filter({ hasText: 'apple' }).getByRole('button').click()
  assert.equal(await ui.title(), '1')
  assert.deepEqual(await ui.getByTestId('total').boundingBox(), { x: 10, y: 50, width: 40, height: 20 })
  assert.equal(await ui.locator('#e').evaluate((el, suffix) => el.value + suffix, '!'), 'a@b.c!')
  const png = await ui.getByTestId('total').screenshot({ path: join(tmpdir(), `fluxwright-el-${process.pid}.png`) })
  assert.deepEqual([png.readUInt32BE(16), png.readUInt32BE(20)], [40, 20]) // PNG width and height
  for (let i = 0; i < 50 && (await ui.pageErrors()).length === 0; i++) await new Promise((r) => setTimeout(r, 20))
  assert.ok((await ui.consoleMessages()).some((m) => m.type === 'log' && m.text === 'ready 1'))
  const [late] = await ui.pageErrors()
  assert.ok(late instanceof Error && late.name === 'TypeError' && late.message === 'late', String(late))
  await assert.rejects(ui.goto('not a url'), /invalid URL: not a url/)
  await ui.close()

  // page.on, page.route (glob, RegExp, function, fallback, unroute) and downloads.
  const live = await browser.newPage()
  const seen = []
  live.on('console', (m) => seen.push(`${m.type}:${m.text}`)).on('pageerror', (e) => seen.push(`${e.name}:${e.message}`))
  await live.route('**/api', (route) => route.fulfill({ json: { fake: true } }))
  await live.route('**/api', (route) => route.fallback()) // added last, runs first, passes it on
  await live.route(/\/hdr$/, (route, request) => route.continue({ headers: { ...request.headers(), 'x-test': 'routed' } }))
  await live.route((url) => url.pathname === '/gone', (route) => route.abort())
  await live.goto(`${base}/hdr`)
  assert.equal(await live.evaluate(() => document.body.textContent), 'routed')
  assert.equal(await live.evaluate(() => fetch('/api').then((r) => r.text())), '{"fake":true}')
  assert.equal(await live.evaluate(() => fetch('/gone').then(() => 'loaded', () => 'failed')), 'failed')
  await live.unroute('**/api')
  assert.equal(await live.evaluate(() => fetch('/api').then((r) => r.text())), '{"real":true}')
  await live.evaluate(() => {
    console.warn('careful')
    setTimeout(() => {
      throw new SyntaxError('oops')
    })
  })
  for (let i = 0; i < 50 && seen.length < 2; i++) await new Promise((r) => setTimeout(r, 20))
  assert.deepEqual(seen, ['warning:careful', 'SyntaxError:oops'])
  await live.goto(base)
  await live.evaluate(() => {
    const a = document.createElement('a')
    a.href = '/report'
    document.body.append(a)
    a.click()
  })
  const download = await live.waitForDownload()
  assert.equal(download.suggestedFilename(), 'report.csv')
  const saved = join(tmpdir(), `fluxwright-${process.pid}`, 'dl', 'report.csv')
  await download.saveAs(saved)
  assert.equal(await readFile(saved, 'utf8'), 'a,b\n1,2\n')
  await live.close()

  // From review: waiting for a download before the click must not block the click; failed,
  // missing or invalid answers let the request through; g/y RegExps match every time; a burst
  // of console output arrives whole.
  const rv = await browser.newPage()
  await rv.goto(base)
  const pending = rv.waitForDownload({ timeout: 10000 })
  await rv.evaluate(() => {
    const a = document.createElement('a')
    a.href = '/report'
    document.body.append(a)
    a.click()
  })
  assert.equal((await pending).suggestedFilename(), 'report.csv')
  const quiet = console.error
  console.error = () => {}
  try {
    await rv.route('**/api', (route) => route.fulfill({ path: join(tmpdir(), 'no-such-file-fluxwright') }))
    assert.equal(await rv.evaluate(() => fetch('/api').then((r) => r.text())), '{"real":true}')
    await rv.unroute('**/api')
    await rv.route('**/api', () => {}) // answers nothing
    assert.equal(await rv.evaluate(() => fetch('/api').then((r) => r.text())), '{"real":true}')
    await rv.unroute('**/api')
    let rejected = null
    await rv.route('**/api', (route) => route.fulfill({ status: 70000 }).catch((e) => (rejected = e.message)))
    assert.equal(await rv.evaluate(() => fetch('/api').then((r) => r.text())), '{"real":true}')
    assert.match(rejected, /status must be between 100 and 599/)
    await rv.unroute('**/api')
    // A URL function that throws is skipped; the request is not left hanging.
    const broken = () => {
      throw new Error('matcher broke')
    }
    await rv.route(broken, (route) => route.fulfill({ body: 'never' }))
    assert.equal(await rv.evaluate(() => fetch('/api').then((r) => r.text())), '{"real":true}')
    await rv.unroute(broken)
  } finally {
    console.error = quiet
  }
  await rv.route(/\/hdr$/g, (route, request) => route.continue({ headers: { ...request.headers(), 'x-test': 'again' } }))
  for (let i = 0; i < 2; i++) {
    await rv.goto(`${base}/hdr`)
    assert.equal(await rv.evaluate(() => document.body.textContent), 'again')
  }
  let count = 0
  rv.on('console', () => count++)
  await rv.evaluate(() => {
    for (let i = 0; i < 600; i++) console.log('n' + i)
  })
  for (let i = 0; i < 250 && count < 600; i++) await new Promise((r) => setTimeout(r, 20))
  assert.equal(count, 600)
  await rv.close()

  // A second route() made while the first is turning interception on returns after it.
  const race = await browser.newPage()
  let firstDone = false
  const first = race.route('**/api', (route) => route.fulfill({ body: 'first' })).then(() => (firstDone = true))
  await race.route('**/other', (route) => route.abort())
  assert.ok(firstDone)
  await first
  await race.goto(base)
  assert.equal(await race.evaluate(() => fetch('/api').then((r) => r.text())), 'first')
  await race.close()

  // A failed start rejects route() and keeps its handler out; the next call starts again.
  const retry = await browser.newPage()
  retry._intercept = async () => {
    throw new Error('intercept failed')
  }
  await assert.rejects(
    retry.route('**/api', (route) => route.fulfill({ body: 'stale' })),
    /intercept failed/,
  )
  delete retry._intercept
  let retried = false
  await retry.route('**/api', (route) => {
    retried = true
    return route.fallback()
  })
  await retry.goto(base)
  assert.equal(await retry.evaluate(() => fetch('/api').then((r) => r.text())), '{"real":true}')
  assert.ok(retried)
  await retry.close()

  // One browser holds 8 pages; the 300 started after them wait their turn instead of failing
  // (the engine used to refuse jobs past 256 waiting, and give up after 30 s).
  const held = await Promise.all(Array.from({ length: 8 }, () => browser.newPage()))
  let settled = 0
  const waiting = Array.from({ length: 300 }, () =>
    browser
      .newPage()
      .then((page) => page.close())
      .finally(() => settled++),
  )
  await new Promise((resolve) => setTimeout(resolve, 500))
  assert.equal(settled, 0)
  await Promise.all(held.map((page) => page.close()))
  await Promise.all(waiting)
  // With queueTimeout, a job that cannot get a slot in time fails.
  const hurried = await fluxwright.chromium.launch({ maxBrowsers: 1, queueTimeout: 300 })
  try {
    const busy = await Promise.all(Array.from({ length: 8 }, () => hurried.newPage()))
    await assert.rejects(hurried.newPage(), /acquire a page lease/)
    await Promise.all(busy.map((page) => page.close()))
  } finally {
    await hurried.close()
  }

  // Left open on purpose: Node drops it at exit, outside the engine's runtime (0.2.1 crashed there).
  await browser.newPage()
} finally {
  await browser.close()
  server.close()
}
console.log('smoke ok')
