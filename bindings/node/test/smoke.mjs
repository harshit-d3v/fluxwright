// Smoke test of the published entry point: `npm test` after `npm run build`. Needs Chrome or
// chrome-headless-shell (see the README).
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { once } from 'node:events'
import { chmodSync, statSync } from 'node:fs'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import http from 'node:http'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import fluxwright from '../addon.js'
import { decodePng } from '../visual.js'

// Echoes the user agent and languages; /login sets a cookie; /report is a download; /hdr echoes
// x-test; /api is the real API that routes stand in for.
let navHits = 0
const server = http.createServer((req, res) => {
  if (req.url === '/nav-a') {
    navHits += 1
    res.setHeader('Content-Type', 'text/html')
    return res.end(`<title>NavA-${navHits}</title><p id="p">A</p>`)
  }
  if (req.url === '/nav-redir') {
    res.statusCode = 302
    res.setHeader('Location', '/nav-b')
    return res.end()
  }
  if (req.url === '/nav-b') {
    res.setHeader('Content-Type', 'text/html')
    return res.end('<title>NavB</title><div id="b">B</div>')
  }
  if (req.url === '/nav-slow') {
    return setTimeout(() => {
      res.setHeader('Content-Type', 'text/plain')
      res.end('slow-ok')
    }, 250)
  }
  if (req.url === '/nav-delayed') {
    res.setHeader('Content-Type', 'text/html')
    return res.end('<script>setTimeout(() => { location.href = "/nav-done" }, 80)</script>')
  }
  if (req.url === '/nav-done') {
    res.setHeader('Content-Type', 'text/html')
    return res.end('<title>NavDone</title>')
  }
  if (req.url === '/init-page') {
    res.setHeader('Content-Type', 'text/html')
    return res.end(
      '<title>InitPage</title>' +
        '<script>window.__seenAtParse = { fromStr: window.__initStr, fromFn: window.__initFn, hasBound: typeof window.ctxFn === "function" }</script>' +
        '<p id="tag-target">Hello</p>',
    )
  }
  if (req.url === '/tag.js') {
    res.setHeader('Content-Type', 'application/javascript')
    return res.end('window.__urlScript = (window.__urlScript || 0) + 1;')
  }
  if (req.url === '/tag.css') {
    res.setHeader('Content-Type', 'text/css')
    return res.end('#tag-target { color: rgb(255, 0, 0); }')
  }
  if (req.url === '/missing-tag.js') {
    res.statusCode = 404
    return res.end('missing')
  }
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
  if (req.url === '/auth') {
    if (req.headers.authorization !== `Basic ${Buffer.from('u:p').toString('base64')}`) {
      res.statusCode = 401
      res.setHeader('WWW-Authenticate', 'Basic realm="test"')
      return res.end('denied')
    }
    res.setHeader('Content-Type', 'text/plain')
    return res.end('authed')
  }
  if (req.url === '/csp') {
    res.setHeader('Content-Security-Policy', "script-src 'none'")
    res.setHeader('Content-Type', 'text/html')
    return res.end('<p id="n">0</p><script>document.getElementById("n").textContent="1"</script>')
  }
  if (req.url === '/opener-page') {
    res.setHeader('Content-Type', 'text/html')
    return res.end(
      '<title>Opener</title>' +
        '<button id="open-btn" onclick="window.open(\'/popup-one\', \'_blank\')">Open</button>' +
        '<a id="blank-link" href="/popup-two" target="_blank">Link</a>',
    )
  }
  if (req.url === '/popup-one') {
    res.setHeader('Content-Type', 'text/html')
    return res.end('<title>Popup1</title><p id="pop">popup-one</p>')
  }
  if (req.url === '/popup-two') {
    res.setHeader('Content-Type', 'text/html')
    return res.end('<title>Popup2</title><p id="pop2">popup-two</p>')
  }
  if (req.url === '/frames-host') {
    const port = server.address().port
    res.setHeader('Content-Type', 'text/html')
    return res.end(
      `<title>FramesHost</title>` +
        `<iframe id="same" name="same-fr" src="http://127.0.0.1:${port}/same-child"></iframe>` +
        `<iframe id="cross" name="cross-fr" src="http://localhost:${port}/cross-child"></iframe>`,
    )
  }
  if (req.url === '/same-child') {
    res.setHeader('Content-Type', 'text/html')
    return res.end('<title>SameTitle</title><button id="sb" onclick="this.textContent=\'clicked-same\'">SameBtn</button>')
  }
  if (req.url === '/same-next') {
    res.setHeader('Content-Type', 'text/html')
    return res.end('<title>SameNext</title><p id="sn">navigated-same</p>')
  }
  if (req.url === '/cross-child') {
    res.setHeader('Content-Type', 'text/html')
    return res.end('<title>CrossTitle</title><button id="cb" onclick="this.textContent=\'clicked-cross\'">CrossBtn</button>')
  }
  if (req.url === '/login') res.setHeader('Set-Cookie', 'sid=s3cret; Path=/')
  res.setHeader('Content-Type', 'text/html')
  res.end(`<title>${req.headers['user-agent']}|${req.headers['accept-language']}</title>`)
})
server.listen(0, '127.0.0.1')
await once(server, 'listening')
server.unref()
const base = `http://127.0.0.1:${server.address().port}`

// A pool that could never start a browser fails at once, and so does NaN (an unset env var, say),
// which napi would read as 0.
await assert.rejects(fluxwright.chromium.launch({ maxBrowsers: 0 }), /at least 1/)
await assert.rejects(fluxwright.chromium.launch({ maxBrowsers: NaN }), /NaN/)

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
  // Options Fluxwright does not implement throw instead of being ignored.
  await assert.rejects(page.waitForSelector('p', { state: 'gone' }), TypeError)
  await page.waitForSelector('p', { state: 'visible' })
  // The other states: hidden and detached wait for it to go, attached takes a hidden element.
  await page.waitForSelector('#never', { state: 'hidden' })
  await page.locator('#never').waitFor({ state: 'detached' })
  await assert.rejects(page.waitForSelector('p', { state: 'hidden', timeout: 300 }))
  await page.evaluate(() => setTimeout(() => (document.querySelector('p').style.display = 'none'), 100))
  await page.waitForSelector('p', { state: 'hidden' })
  await page.waitForSelector('p', { state: 'attached' })
  await assert.rejects(page.locator('p').waitFor({ timeout: 300 }))
  await page.evaluate(() => (document.querySelector('p').style.display = ''))
  assert.throws(() => page.locator('p', { bogusOption: 'x' }), TypeError)
  await assert.rejects(page.click('p', { noWaitAfter: true }), TypeError)
  await assert.rejects(page.click('p', { button: 'fourth' }), TypeError)
  await assert.rejects(page.click('p', { modifiers: ['Bogus'] }), TypeError)
  await assert.rejects(page.screenshot({ path: join(tmpdir(), 'fluxwright.gif') }), TypeError)
  await assert.rejects(page.screenshot({ path: join(tmpdir(), 'fluxwright.webp') }), TypeError)
  await assert.rejects(page.screenshot({ type: 'webp' }), TypeError)
  await assert.rejects(page.screenshot({ animations: 'bogus' }), TypeError)
  await assert.rejects(page.screenshot({ caret: 'bogus' }), TypeError)
  await assert.rejects(page.screenshot({ maskColor: 'red; background:url(x)' }), TypeError)
  await assert.rejects(page.screenshot({ mask: '#secret' }), TypeError)
  await assert.rejects(page.screenshot({ mask: [123] }), TypeError)
  await assert.rejects(page.screenshot({ annotate: 'yes' }), TypeError)
  await assert.rejects(page.screenshot({ bogusOption: true }), TypeError)
  await assert.rejects(page.screenshot({ clip: { x: 0, y: 0, width: 10, height: 10, extra: 1 } }), TypeError)
  await assert.rejects(page.screenshot({ type: 'png', quality: 50 }), /JPEG only/)
  await assert.rejects(page.pdf({ format: 'A9' }), /unknown format/)
  await assert.rejects(page.pdf({ landscpe: true }), TypeError)
  await assert.rejects(page.pdf({ margin: { bogus: 10 } }), TypeError)
  await assert.rejects(
    browser.newPage({ httpCredentials: { username: 'u', password: 'p', send: 'always' } }),
    TypeError,
  )
  // A call's timeout, then the page's default, replace the engine's 10 s.
  let began = Date.now()
  await assert.rejects(page.waitForSelector('#never', { timeout: 300 }))
  await assert.rejects(page.locator('#never').click({ timeout: 300 }))
  assert.ok(Date.now() - began < 5000, `per-call timeouts took ${Date.now() - began} ms`)
  page.setDefaultTimeout(300)
  began = Date.now()
  await assert.rejects(page.click('#never'))
  assert.ok(Date.now() - began < 5000, `the default timeout took ${Date.now() - began} ms`)
  await page.click('p', { timeout: 5000 })
  await page.close()

  // Per-page emulation, with Playwright's option names.
  const emulated = await browser.newPage({
    userAgent: 'FluxSmoke/1',
    locale: 'fr-FR',
    timezoneId: 'Europe/Paris',
    viewport: { width: 600, height: 500 },
    deviceScaleFactor: 2,
    colorScheme: 'dark',
    hasTouch: true,
    extraHTTPHeaders: { 'x-flux': '1' },
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
        navigator.maxTouchPoints > 0,
      ].join('|'),
    ),
    'fr-FR|Europe/Paris|600|2|true|true',
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
  // JPEG from the path's extension, a clip, and a PDF.
  const jpegPath = join(tmpdir(), `fluxwright-el-${process.pid}.jpg`)
  const jpeg = await ui.getByTestId('total').screenshot({ path: jpegPath, quality: 60 })
  assert.deepEqual([...jpeg.subarray(0, 3)], [0xff, 0xd8, 0xff])
  assert.deepEqual(await readFile(jpegPath), jpeg)
  const clip = await ui.screenshot({ clip: { x: 10, y: 50, width: 40, height: 20 } })
  assert.deepEqual([clip.readUInt32BE(16), clip.readUInt32BE(20)], [40, 20])
  const clear = await ui.screenshot({ omitBackground: true })
  assert.equal(clear.subarray(0, 4).toString('latin1'), '\x89PNG')
  const pdfPath = join(tmpdir(), `fluxwright-${process.pid}.pdf`)
  const pdf = await ui.pdf({
    path: pdfPath,
    format: 'A4',
    landscape: true,
    printBackground: true,
    scale: 0.9,
    pageRanges: '1',
    margin: { top: 20, bottom: '1cm' },
    displayHeaderFooter: true,
    headerTemplate: '<span class=title></span>',
    footerTemplate: '<span class=pageNumber></span>',
    outline: true,
    tagged: true,
  })
  assert.equal(pdf.subarray(0, 5).toString(), '%PDF-')
  assert.deepEqual(await readFile(pdfPath), pdf)
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

  // A listener that throws (Playwright's `msg.text()` on a `{ type, text }` message) or rejects
  // is logged: the process lives on and the other listeners still run.
  const quietErrors = console.error
  const logged = []
  console.error = (...args) => logged.push(args.join(' '))
  try {
    const strict = await browser.newPage()
    const got = []
    strict.on('console', (m) => m.text()).on('console', async () => {
      throw new Error('async boom')
    })
    strict.on('console', (m) => got.push(m.text))
    await strict.evaluate(() => console.log('hello'))
    for (let i = 0; i < 50 && (got.length < 1 || logged.length < 2); i++) await new Promise((r) => setTimeout(r, 20))
    assert.deepEqual(got, ['hello'])
    assert.equal(logged.length, 2, logged.join('\n'))
    await strict.close()
  } finally {
    console.error = quietErrors
  }

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
  const pending = rv.waitForEvent('download', { timeout: 10000 })
  await rv.evaluate(() => {
    const a = document.createElement('a')
    a.href = '/report'
    document.body.append(a)
    a.click()
  })
  assert.equal((await pending).suggestedFilename(), 'report.csv')
  await rv.waitForTimeout(10)
  await assert.rejects(() => rv.waitForTimeout(-1), TypeError)
  const quiet = console.error
  const warn = console.warn
  let warning = ''
  console.error = () => {}
  console.warn = (message) => (warning = message)
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
    // An answer after the handler returned is ignored with a warning, not a rejection that would
    // crash Node; the request went on without it.
    let late = null
    await rv.route('**/api', (route) => {
      late = new Promise((resolve) => setTimeout(resolve, 50)).then(() => route.fulfill({ body: 'late' }))
    })
    assert.equal(await rv.evaluate(() => fetch('/api').then((r) => r.text())), '{"real":true}')
    await late
    assert.match(warning, /after its handler returned/)
    await rv.unroute('**/api')
  } finally {
    console.error = quiet
    console.warn = warn
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

  // Network events, waitForResponse / waitForRequest, and dialogs.
  const net = await browser.newPage()
  const seenNet = []
  const path = (r) => new URL(r.url()).pathname
  net.on('request', (r) => seenNet.push(`request ${r.method()} ${path(r)}`))
  net.on('response', (r) => seenNet.push(`response ${r.status()} ${path(r)}`))
  net.on('requestfinished', (r) => seenNet.push(`finished ${path(r)}`))
  net.on('requestfailed', (r) => seenNet.push(`failed ${path(r)} ${r.failure()?.errorText}`))
  // One Request object per request, as in Playwright, so in-flight trackers drain.
  const requested = []
  const inFlight = new Set()
  net.on('request', (r) => {
    requested.push(r)
    inFlight.add(r)
  })
  net.on('requestfinished', (r) => inFlight.delete(r))
  net.on('requestfailed', (r) => inFlight.delete(r))
  await net.goto(base)
  const [api] = await Promise.all([net.waitForResponse('**/api'), net.evaluate(() => fetch('/api').then((r) => r.text()))])
  assert.equal(api.status(), 200)
  assert.ok(api.ok())
  assert.deepEqual(await api.json(), { real: true })
  assert.equal(api.request().method(), 'GET')
  assert.equal(api.headers()['content-type'], 'application/json')
  await net.route('**/gone', (route) => route.abort())
  await net.evaluate(() => fetch('/gone').catch(() => 'failed'))
  for (let i = 0; i < 50 && !seenNet.some((e) => e.startsWith('failed')); i++) await new Promise((r) => setTimeout(r, 20))
  assert.ok(seenNet.includes('request GET /'), seenNet.join('\n'))
  assert.ok(seenNet.includes('response 200 /api'), seenNet.join('\n'))
  assert.ok(seenNet.includes('finished /api'), seenNet.join('\n'))
  assert.ok(seenNet.includes('failed /gone net::ERR_FAILED'), seenNet.join('\n'))
  assert.ok(requested.includes(api.request()), 'response.request() is the request event object')
  assert.ok(![...inFlight].some((r) => ['/api', '/gone'].includes(path(r))), [...inFlight].map(path).join(' '))
  const byPredicate = net.waitForRequest((r) => r.url().endsWith('/hdr'))
  await net.evaluate(() => fetch('/hdr'))
  assert.equal((await byPredicate).method(), 'GET')
  await assert.rejects(net.waitForResponse('**/never', { timeout: 200 }), /timeout 200ms exceeded/)
  // Without a listener a dialog is dismissed; a listener answers it; one it leaves is dismissed.
  assert.equal(await net.evaluate(() => confirm('sure?')), false)
  net.on('dialog', (d) => (d.type() === 'prompt' ? d.accept(`${d.message()}!`) : undefined))
  assert.equal(await net.evaluate(() => prompt('name')), 'name!')
  assert.equal(await net.evaluate(() => confirm('sure?')), false)
  // A beforeunload one it leaves is accepted, as with no listener: leaving the page works (the
  // click arms it: Chrome wants a user gesture).
  const arm = `<button onclick="onbeforeunload = (e) => { e.preventDefault(); e.returnValue = '' }">arm</button>`
  await net.goto(`data:text/html,${encodeURIComponent(arm)}`)
  await net.click('button')
  await net.goto('data:text/html,<title>left</title>')
  assert.equal(await net.title(), 'left')
  await net.close()

  // browser.newContext(): its options, cookies and routes apply to its one page.
  const context = await browser.newContext({
    userAgent: 'CtxSmoke/1',
    baseURL: base,
    bypassCSP: true,
    httpCredentials: { username: 'u', password: 'p' },
  })
  await context.addCookies([{ name: 'pre', value: 'yes', url: base }])
  await context.route('**/api', (route) => route.fulfill({ body: 'from the context' }))
  assert.deepEqual(context.pages(), [])
  const inContext = await context.newPage()
  await inContext.goto('/login')
  assert.match(await inContext.title(), /^CtxSmoke\/1/)
  assert.equal(await inContext.evaluate(() => fetch('/api').then((r) => r.text())), 'from the context')
  assert.deepEqual(context.pages(), [inContext])
  assert.ok((await context.cookies()).some((c) => c.name === 'sid'))
  assert.ok((await context.cookies()).some((c) => c.name === 'pre'))
  await context.addCookies([{ name: 'mid', value: '2', url: base }])
  assert.ok((await context.cookies()).some((c) => c.name === 'mid'))
  await context.clearCookies()
  assert.equal((await context.cookies()).length, 0)
  await inContext.goto('/csp')
  assert.equal(await inContext.locator('#n').textContent(), '1')
  await inContext.goto('/auth')
  assert.equal(await inContext.locator('body').textContent(), 'authed')
  await inContext.goto('data:text/html,<style>@media print { p { color: rgb(255, 0, 0); } }</style><img alt="Site Logo" title="Hover Info"><p>T</p>')
  assert.equal(await inContext.getByAltText('Site Logo', { exact: true }).evaluate((el) => el.tagName), 'IMG')
  assert.equal(await inContext.getByTitle('hover').evaluate((el) => el.getAttribute('alt')), 'Site Logo')
  await inContext.emulateMedia({ media: 'print', colorScheme: 'dark' })
  assert.equal(await inContext.evaluate(() => getComputedStyle(document.querySelector('p')).color), 'rgb(255, 0, 0)')
  assert.equal(await inContext.evaluate(() => matchMedia('(prefers-color-scheme: dark)').matches), true)
  await inContext.emulateMedia({ media: null, colorScheme: null })
  await assert.rejects(fluxwright.chromium.connectOverCDP('ws://127.0.0.1:1', { slowMo: 1 }), TypeError)
  await assert.rejects(fluxwright.chromium.connectOverCDP('ws://127.0.0.1:1'))
  const inContext2 = await context.newPage()
  assert.equal(context.pages().length, 2)
  await context.addCookies([{ name: 'shared', value: 'v2', url: base }])
  await inContext2.goto('/')
  assert.equal(await inContext2.evaluate(() => document.cookie), 'shared=v2')
  await inContext.bringToFront()
  await context.close()
  assert.ok(inContext.isClosed())
  assert.ok(inContext2.isClosed())
  assert.deepEqual(context.pages(), [])
  await assert.rejects(context.newPage(), /context is closed/)

  // Track 1: pointer options, mouse, keyboard, forms, file uploads, and touch.
  const rich = await browser.newPage({ hasTouch: true })
  await rich.goto(
    'data:text/html,<body style="margin:0;height:2000px">' +
      '<button id="btn" style="border:0;padding:0;position:relative;width:80px;height:40px" ' +
      'oncontextmenu="event.preventDefault();window.__ctx=[event.button,event.shiftKey,Math.round(event.offsetX),Math.round(event.offsetY)]" ' +
      'ondblclick="window.__dbl=(window.__dbl||0)+1" ' +
      'onmouseenter="window.__hover=true" ' +
      'ontouchstart="window.__tap=(window.__tap||0)+1">Btn</button>' +
      '<div id="covered" role="button" aria-disabled="true" onclick="window.__forced=true" style="position:relative;width:40px;height:20px">C</div>' +
      '<div id="dragsrc" style="position:absolute;left:120px;top:20px;width:20px;height:20px" onmousedown="window.__dragStart=true"></div>' +
      '<div id="dragdst" style="position:absolute;left:220px;top:20px;width:20px;height:20px" onmouseup="window.__dragEnd=true"></div>' +
      '<input id="inp" value="old" onfocus="window.__foc=true" onblur="window.__foc=false" onkeydown="window.__keys=(window.__keys||[]);window.__keys.push(event.key)">' +
      '<input id="chk" type="checkbox"><input id="rad" type="radio" name="r">' +
      '<div id="sw" role="switch" tabindex="0" aria-checked="false" onclick="this.setAttribute(\'aria-checked\', String(this.getAttribute(\'aria-checked\')!==\'true\'))">Sw</div>' +
      '<select id="sel" multiple><option value="a">Alpha</option><option value="b">Beta</option><option value="c">Gamma</option></select>' +
      '<input id="file" type="file" multiple>' +
      '<div id="below" style="margin-top:1400px">End</div>' +
      '<script>window.addEventListener("wheel", e => { window.__wheel = e.deltaY; if (window.__resolveWheel) window.__resolveWheel(e.deltaY); }, { passive: false });</script></body>',
  )
  await rich.click('#btn', { trial: true, button: 'right' })
  assert.equal(await rich.evaluate(() => window.__ctx), null)
  await rich.click('#btn', { button: 'right', modifiers: ['Shift'], position: { x: 10, y: 12 } })
  assert.deepEqual(await rich.evaluate(() => window.__ctx), [2, true, 10, 12])
  await rich.dblclick('#btn')
  assert.equal(await rich.evaluate(() => window.__dbl), 1)
  await rich.hover('#btn')
  assert.equal(await rich.evaluate(() => window.__hover), true)
  await rich.click('#covered', { force: true })
  assert.equal(await rich.evaluate(() => window.__forced), true)
  await rich.dragAndDrop('#dragsrc', '#dragdst')
  assert.deepEqual(await rich.evaluate(() => [window.__dragStart, window.__dragEnd]), [true, true])

  await rich.locator('#below').scrollIntoViewIfNeeded()
  assert.ok((await rich.evaluate(() => window.scrollY)) > 500)
  await rich.evaluate(() => window.scrollTo(0, 0))
  await rich.mouse.move(10, 10)
  await rich.mouse.wheel(0, 120)
  assert.equal(
    await rich.evaluate(
      () =>
        window.__wheel ??
        new Promise((res, rej) => {
          window.__resolveWheel = res
          setTimeout(() => rej(new Error('wheel timeout')), 3000)
        }),
    ),
    120,
  )

  await rich.focus('#inp')
  assert.equal(await rich.evaluate(() => window.__foc), true)
  await rich.locator('#inp').clear()
  assert.equal(await rich.evaluate(() => inp.value), '')
  await rich.locator('#inp').pressSequentially('hello')
  await rich.press('#inp', 'ControlOrMeta+a')
  await rich.press('#inp', 'Backspace')
  await rich.keyboard.insertText('世')
  await rich.press('#inp', 'Enter')
  assert.equal(await rich.evaluate(() => inp.value), '世')
  assert.ok((await rich.evaluate(() => window.__keys)).includes('Enter'))
  await rich.locator('#inp').blur()
  assert.equal(await rich.evaluate(() => window.__foc), false)

  await rich.check('#chk')
  assert.equal(await rich.evaluate(() => chk.checked), true)
  await rich.uncheck('#chk')
  assert.equal(await rich.evaluate(() => chk.checked), false)
  await rich.setChecked('#sw', true)
  assert.equal(await rich.evaluate(() => sw.getAttribute('aria-checked')), 'true')
  await assert.rejects(rich.locator('#rad').uncheck(), /cannot uncheck a radio/)

  assert.deepEqual(await rich.selectOption('#sel', [{ label: 'Beta' }, { index: 2 }]), ['b', 'c'])
  assert.deepEqual(
    await rich.evaluate(() => [...sel.selectedOptions].map((o) => o.value)),
    ['b', 'c'],
  )

  const uploadPath = join(tmpdir(), `fluxwright-upload-${process.pid}.txt`)
  await writeFile(uploadPath, 'from-disk')
  await rich.setInputFiles('#file', uploadPath)
  assert.equal(await rich.evaluate(() => file.files[0]?.name), `fluxwright-upload-${process.pid}.txt`)
  await rich.setInputFiles('#file', [])
  assert.equal(await rich.evaluate(() => file.files.length), 0)
  await rich.setInputFiles('#file', { name: 'mem.txt', mimeType: 'text/plain', buffer: Buffer.from('mem') })
  assert.equal(await rich.evaluate(() => file.files[0]?.name), 'mem.txt')

  const [fc] = await Promise.all([rich.waitForFileChooser(), rich.click('#file')])
  assert.equal(fc.isMultiple(), true)
  await fc.setFiles({ name: 'chosen.txt', mimeType: 'text/plain', buffer: Buffer.from('chosen') })
  assert.equal(await rich.evaluate(() => file.files[0]?.name), 'chosen.txt')

  await rich.tap('#btn')
  await rich.touchscreen.tap(15, 15)
  assert.equal(await rich.evaluate(() => window.__tap), 2)
  await rich.close()

  // close() does not wait for calls still running on the page (this one would run until the
  // action timeout): they reject at once, and so do pending waits.
  const closing = await browser.newPage()
  const openCall = assert.rejects(closing.waitForSelector('#never'), /page closed/)
  const openWait = assert.rejects(closing.waitForResponse('**/never', { timeout: 0 }), /page closed/)
  const closeStarted = Date.now()
  await closing.close()
  assert.ok(Date.now() - closeStarted < 5000, 'close() waited for a running call')
  await openCall
  await openWait

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
  // With queueTimeout, a job that cannot get a page in time fails. The limit also covers
  // starting a browser and making the context, which a slow runner can outlast, so fill the
  // slots one at a time with retries; a page that arrives late is released.
  const hurried = await fluxwright.chromium.launch({ maxBrowsers: 1, queueTimeout: 2000 })
  try {
    const busy = []
    for (let tries = 0; busy.length < 8 && tries < 30; tries++) {
      const page = await hurried.newPage().catch(() => null)
      if (page) busy.push(page)
    }
    assert.equal(busy.length, 8, 'could not fill the browser')
    await assert.rejects(hurried.newPage(), /acquire a page lease/)
    await Promise.all(busy.map((page) => page.close()))
  } finally {
    await hurried.close()
  }

  // A page you forget is cleaned up once Node collects it, even when its listeners and routes use
  // it: the native side reaches them only through a WeakRef. Needs --expose-gc, as npm test runs.
  if (globalThis.gc) {
    const small = await fluxwright.chromium.launch({ maxBrowsers: 1, queueTimeout: 10000 })
    try {
      for (let i = 0; i < 8; i++) {
        const page = await small.newPage()
        page.on('console', () => page.isClosed()).on('request', () => page.isClosed())
        page.on('dialog', (dialog) => (page.isClosed() ? undefined : dialog.dismiss()))
        await page.route('**/never', (route) => (page.isClosed() ? route.abort() : route.fallback()))
      }
      for (let i = 0; i < 5; i++) {
        globalThis.gc()
        await new Promise((resolve) => setTimeout(resolve, 100))
      }
      // All 8 slots of its one browser were taken: only a collected page can give one back.
      await (await small.newPage()).close()
    } finally {
      await small.close()
    }
  }

  // Locator reading, filtering, combinators, XPath, extended getByRole, and strict mode.
  {
    const lr = await browser.newPage()
    await lr.goto('data:text/html,' + encodeURIComponent(`<!doctype html>
      <h2 aria-level="2">Sub Header</h2>
      <ul id="list">
        <li><b>Apple</b> <span>red</span></li>
        <li><em>Pear</em> <span>green</span></li>
        <li style="display:none">HiddenItem</li>
      </ul>
      <input id="ro" readonly value="locked" data-kind="ro">
      <input id="ed" value="ok">
      <input id="dis" disabled>
      <input id="chk" type="checkbox" checked>
      <div id="ce" contenteditable="true">Hi <b>there</b></div>
      <button id="tog" aria-pressed="true" aria-expanded="false">Toggle</button>
      <button id="hid" style="display:none">Secret</button>
      <div role="tab" aria-selected="true">TabA</div>
      <div id="parent"><span id="child">Kid</span></div>
      <p class="m">First</p><p class="m">Second</p>`))
    const list = lr.locator('#list li')
    assert.equal(await list.count(), 3)
    const all = await list.all()
    assert.equal(all.length, 3)
    assert.equal(await all[1].innerText(), 'Pear green')
    assert.deepEqual(await list.allTextContents(), ['Apple red', 'Pear green', 'HiddenItem'])
    assert.equal((await list.allInnerTexts()).length, 3)
    assert.equal(await list.first().innerHTML(), '<b>Apple</b> <span>red</span>')
    assert.equal(await lr.locator('#ed').inputValue(), 'ok')
    assert.equal(await lr.locator('#ro').getAttribute('data-kind'), 'ro')
    assert.equal(await lr.locator('#ro').getAttribute('missing'), null)
    assert.equal(await lr.locator('#missing').isVisible(), false)
    assert.equal(await lr.locator('#missing').isHidden(), true)
    assert.equal(await lr.locator('#ed').isEnabled(), true)
    assert.equal(await lr.locator('#dis').isDisabled(), true)
    assert.equal(await lr.locator('#chk').isChecked(), true)
    assert.equal(await lr.locator('#ed').isEditable(), true)
    assert.equal(await lr.locator('#ro').isEditable(), false)
    assert.equal(await lr.locator('#ce').isEditable(), true)

    assert.equal(await lr.innerText('.m'), 'First')
    await assert.rejects(() => lr.locator('.m').innerText(), /strict mode violation/)
    await assert.rejects(() => lr.innerText('.m', { strict: true }), /strict mode violation/)
    await assert.rejects(() => lr.locator('.m').isVisible(), /strict mode violation/)

    assert.equal(await list.filter({ has: lr.locator('b') }).innerText(), 'Apple red')
    assert.equal(await lr.locator('#list li', { hasNot: lr.locator('b'), hasNotText: /Hidden/ }).innerText(), 'Pear green')
    assert.equal(await list.filter({ hasText: /^Pear/i }).innerText(), 'Pear green')
    assert.equal(await lr.locator('input').and(lr.locator('#ed')).inputValue(), 'ok')
    assert.equal(await lr.locator('#ed').or(lr.locator('#chk')).count(), 2)
    assert.equal(await lr.locator('//button[@id="tog"]').innerText(), 'Toggle')
    assert.equal(await lr.locator('#child').locator('..').getAttribute('id'), 'parent')
    assert.equal(await lr.getByRole('heading', { level: 2, name: /^Sub/ }).innerText(), 'Sub Header')
    assert.equal(await lr.getByRole('button', { pressed: true, expanded: false }).getAttribute('id'), 'tog')
    assert.equal(await lr.getByRole('button', { name: 'Secret', includeHidden: true }).getAttribute('id'), 'hid')
    assert.equal(await lr.getByRole('checkbox', { checked: true }).getAttribute('id'), 'chk')
    assert.equal(await lr.getByRole('tab', { selected: true }).innerText(), 'TabA')
    assert.equal(await lr.frameLocator('#nope').locator('div').count(), 0)
    await lr.close()
  }

  // Navigation and wait helpers (url, reload, history, setContent, waitForLoadState, waitForURL, waitForFunction).
  {
    const nav = await browser.newPage({ baseURL: base })
    assert.equal(nav.url(), 'about:blank')
    await nav.goBack()
    await nav.goForward()
    await nav.goto('/nav-a')
    assert.equal(nav.url(), `${base}/nav-a`)
    assert.equal(await nav.title(), 'NavA-1')
    await nav.reload()
    assert.equal(await nav.title(), 'NavA-2')
    await nav.goto('/nav-redir')
    assert.equal(nav.url(), `${base}/nav-b`)
    await nav.goBack()
    assert.equal(nav.url(), `${base}/nav-a`)
    await nav.goForward()
    assert.equal(nav.url(), `${base}/nav-b`)
    await nav.evaluate(() => history.pushState({}, '', '/nav-b#s'))
    assert.equal(nav.url(), `${base}/nav-b#s`)
    await nav.goBack()
    assert.equal(nav.url(), `${base}/nav-b`)

    await nav.setContent('<h1 id="sc">Set</h1><script>window.__sc = 12</script>')
    assert.equal(await nav.innerText('#sc'), 'Set')
    assert.equal(await nav.evaluate(() => window.__sc), 12)

    await nav.goto('/nav-a')
    await nav.waitForLoadState('load')
    await nav.evaluate(() => {
      window.__slowDone = false
      fetch('/nav-slow').then(() => {
        window.__slowDone = true
      })
    })
    await nav.waitForLoadState('networkidle')
    assert.equal(await nav.evaluate(() => window.__slowDone), true)

    await nav.goto('/nav-delayed')
    await nav.waitForURL('**/nav-done')
    assert.equal(nav.url(), `${base}/nav-done`)
    await nav.goto('/nav-delayed')
    await nav.waitForURL(/\/nav-done$/)
    await nav.goto('/nav-delayed')
    await nav.waitForURL((u) => u.pathname === '/nav-done', { waitUntil: 'commit' })
    await assert.rejects(() => nav.waitForURL('/never', { timeout: 300 }), /timed out/)

    await nav.evaluate(() => setTimeout(() => { window.__fn = { n: 5 } }, 60))
    assert.deepEqual(
      await nav.waitForFunction((want) => window.__fn && window.__fn.n + want, 2, { polling: 20 }),
      7,
    )
    assert.equal(await nav.waitForFunction('window.__fn && window.__fn.n'), 5)
    await assert.rejects(() => nav.waitForFunction(() => { throw new Error('fn-boom') }), /fn-boom/)
    await assert.rejects(() => nav.waitForFunction(() => false, undefined, { timeout: 250 }), /timed out/)
    await assert.rejects(() => nav.waitForLoadState('bogus'), TypeError)
    await nav.close()
  }

  // Init scripts, exposed functions, script tags, and style tags (page & context).
  {
    const initPath = join(tmpdir(), `fluxwright-node-init-${process.pid}.js`)
    const tagJsPath = join(tmpdir(), `fluxwright-node-tag-${process.pid}.js`)
    const tagCssPath = join(tmpdir(), `fluxwright-node-tag-${process.pid}.css`)
    await writeFile(initPath, 'window.__initPath = true;')
    await writeFile(tagJsPath, 'window.__fileTag = "from-file";')
    await writeFile(tagCssPath, '#tag-target { margin-left: 17px; }')
    const ctx = await browser.newContext({ baseURL: base })
    try {
      await ctx.exposeFunction('ctxFn', async (n) => n * 3)
      await ctx.addInitScript('window.__initStr = "ok";')
      await ctx.addInitScript((v) => {
        window.__initFn = v.tag
      }, { tag: 99 })

      const p = await ctx.newPage()
      await p.addInitScript({ path: initPath })
      await p.addInitScript({ content: 'window.__initContent = "c";' })
      await p.goto('/init-page')

      assert.deepEqual(await p.evaluate(() => window.__seenAtParse), {
        fromStr: 'ok',
        fromFn: 99,
        hasBound: true,
      })
      assert.deepEqual(await p.evaluate(() => [window.__initPath, window.__initContent]), [true, 'c'])
      assert.equal(await p.evaluate(() => window.ctxFn(14)), 42)

      await p.exposeFunction('withTitle', async (suffix) => `${await p.title()}:${suffix}`)
      assert.equal(await p.evaluate(() => window.withTitle('x')), 'InitPage:x')
      await p.exposeFunction('noReturn', () => {})
      await p.evaluate(() => window.noReturn())
      await p.exposeFunction('boomFn', async () => {
        throw new Error('cb-boom')
      })
      await assert.rejects(() => p.evaluate(() => window.boomFn()), /cb-boom/)
      await assert.rejects(() => p.exposeFunction('ctxFn', () => 1), /already registered/)
      await assert.rejects(() => ctx.exposeFunction('ctxFn', () => 1), /already registered/)

      await p.reload()
      assert.deepEqual(await p.evaluate(() => [window.__initStr, window.__initFn]), ['ok', 99])
      assert.equal(await p.evaluate(() => window.withTitle('r')), 'InitPage:r')

      await p.addScriptTag({ content: 'window.__inlineTag = 7;' })
      await p.addScriptTag({ url: `${base}/tag.js` })
      await p.addScriptTag({ path: tagJsPath })
      await p.addScriptTag({ content: 'window.__modLoaded = true;', type: 'module' })
      await p.waitForFunction(() => window.__modLoaded === true)
      assert.deepEqual(
        await p.evaluate(() => [
          window.__inlineTag,
          window.__urlScript,
          window.__fileTag,
          document.querySelector('script[type="module"]') !== null,
        ]),
        [7, 1, 'from-file', true],
      )
      await assert.rejects(() => p.addScriptTag({ url: `${base}/missing-tag.js` }))
      await assert.rejects(() => p.addScriptTag({}), TypeError)

      await p.addStyleTag({ content: 'body { background-color: rgb(1, 2, 3); }' })
      await p.addStyleTag({ url: `${base}/tag.css` })
      await p.addStyleTag({ path: tagCssPath })
      assert.deepEqual(
        await p.evaluate(() => [
          getComputedStyle(document.body).backgroundColor,
          getComputedStyle(document.querySelector('#tag-target')).color,
          getComputedStyle(document.querySelector('#tag-target')).marginLeft,
        ]),
        ['rgb(1, 2, 3)', 'rgb(255, 0, 0)', '17px'],
      )
    } finally {
      await ctx.close()
      await Promise.all([rm(initPath, { force: true }), rm(tagJsPath, { force: true }), rm(tagCssPath, { force: true })])
    }
  }

  // Multi-tab contexts, popups, and imperative Frame handles (same-origin + cross-origin OOPIF).
  {
    const ctx = await browser.newContext({ baseURL: base, locale: 'fr-FR' })
    try {
      await ctx.addCookies([{ name: 'pre', value: 'yes', url: base }])
      const preState = await ctx.storageState()
      assert.ok(preState.cookies.some((c) => c.name === 'pre'))
      assert.ok((await ctx.cookies()).some((c) => c.name === 'pre'))
      await ctx.addInitScript('window.__ctxInit = "shared";')
      await ctx.exposeFunction('ctxMul', (a, b) => a * b)
      await ctx.route('**/api', (r) => r.fulfill({ body: 'from-ctx-route' }))

      const opener = await ctx.newPage()
      await opener.goto('/opener-page')
      const seenPopups = []
      opener.on('popup', (p) => seenPopups.push(p))
      const [pop1] = await Promise.all([opener.waitForPopup(), opener.click('#open-btn')])
      await pop1.waitForLoadState('load')
      assert.equal(await pop1.opener(), opener)
      assert.ok(ctx.pages().includes(pop1))
      assert.equal(await pop1.locator('#pop').innerText(), 'popup-one')
      assert.deepEqual(
        await pop1.evaluate(async () => [window.__ctxInit, navigator.language, await ctxMul(3, 5)]),
        ['shared', 'fr-FR', 15],
      )
      assert.equal(await pop1.evaluate(() => fetch('/api').then((r) => r.text())), 'from-ctx-route')
      await pop1.close()
      assert.ok(!ctx.pages().includes(pop1))

      const [pop2] = await Promise.all([
        opener.waitForPopup({ predicate: (p) => !p.isClosed() }),
        opener.click('#blank-link'),
      ])
      await pop2.waitForLoadState('load')
      assert.equal(await pop2.locator('#pop2').innerText(), 'popup-two')
      assert.equal(seenPopups.length, 2)
      await pop2.close()

      await opener.goto('/frames-host')
      let loaded = false
      for (let i = 0; i < 100; i++) {
        const fs = opener.frames()
        if (
          fs.some((f) => f.name() === 'same-fr' && f.url().endsWith('/same-child')) &&
          fs.some((f) => f.name() === 'cross-fr' && f.url().endsWith('/cross-child'))
        ) {
          loaded = true
          break
        }
        await new Promise((r) => setTimeout(r, 30))
      }
      assert.ok(loaded, 'frames loaded')
      const main = opener.mainFrame()
      assert.equal(opener.frames().length, 3)
      assert.equal(main.childFrames().length, 2)
      const same = opener.frame('same-fr')
      const cross = opener.frame({ url: /cross-child/ })
      assert.ok(same && cross)
      assert.equal(same.parentFrame(), main)
      assert.equal(cross.parentFrame(), main)
      assert.equal(same.isDetached(), false)
      assert.equal(cross.isDetached(), false)

      await same.locator('#sb').click()
      assert.equal(await same.locator('#sb').innerText(), 'clicked-same')
      assert.equal(await same.title(), 'SameTitle')
      await cross.locator('#cb').click()
      assert.equal(await cross.locator('#cb').innerText(), 'clicked-cross')
      assert.equal(await cross.title(), 'CrossTitle')
      assert.match(await cross.content(), /clicked-cross/)
      assert.equal(await cross.evaluate((x) => x + 10, 5), 15)

      const otherCtx = await browser.newContext()
      try {
        await assert.rejects(() => otherCtx.newCDPSession(opener), /belong to this context/)
      } finally {
        await otherCtx.close()
      }
      const cdp = await ctx.newCDPSession(opener)
      const consolePromise = new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('timed out waiting for CDP console event')), 5000)
        cdp.on('Runtime.consoleAPICalled', (ev) => {
          if (ev?.args?.[0]?.value === 'cdp-node') {
            clearTimeout(timer)
            resolve(ev)
          }
        })
      })
      await cdp.send('Runtime.enable')
      const evalRes = await cdp.send('Runtime.evaluate', {
        expression: "console.log('cdp-node'); 6 * 7",
        returnByValue: true,
      })
      assert.equal(evalRes.result.value, 42)
      await consolePromise
      await cdp.detach()
      await assert.rejects(() => cdp.send('Runtime.evaluate', { expression: '1' }))
      assert.equal(await opener.evaluate(() => 3 + 4), 7)

      await assert.rejects(() => ctx.newCDPSession(same), /separate CDP session/)
      const crossCdp = await ctx.newCDPSession(cross)
      const crossEval = await crossCdp.send('Runtime.evaluate', {
        expression: "document.getElementById('cb').textContent",
        returnByValue: true,
      })
      assert.equal(crossEval.result.value, 'clicked-cross')
      await crossCdp.detach()

      await same.goto('/same-next')
      assert.equal(await same.locator('#sn').innerText(), 'navigated-same')

      await opener.evaluate(() => document.getElementById('same').remove())
      for (let i = 0; i < 50 && !same.isDetached(); i++) {
        await new Promise((r) => setTimeout(r, 20))
      }
      assert.equal(same.isDetached(), true)
    } finally {
      await ctx.close()
    }
  }

  // PR 4B: cookies(urls), clearCookies(filter), storageState({ indexedDB }), APIRequestContext, route.fetch()
  {
    const http = await import('node:http')
    const srv = http.createServer(async (req, res) => {
      const u = new URL(req.url, 'http://127.0.0.1')
      const chunks = []
      for await (const c of req) chunks.push(c)
      const rawBody = Buffer.concat(chunks).toString('utf8')
      if (u.pathname === '/idb') {
        res.writeHead(200, { 'content-type': 'text/html' })
        res.end('<p id="idb-page">ok</p>')
      } else if (u.pathname === '/api/login') {
        res.writeHead(200, {
          'content-type': 'application/json',
          'set-cookie': ['api_tok=node99; Path=/; HttpOnly', 'vis=yes; Path=/'],
        })
        res.end(JSON.stringify({ method: req.method, ctype: req.headers['content-type'] || '', body: rawBody, xCtx: req.headers['x-ctx'] || '' }))
      } else if (u.pathname === '/api/echo') {
        res.writeHead(200, { 'content-type': 'application/json' })
        res.end(JSON.stringify({ cookie: req.headers.cookie || '', query: u.searchParams.get('q') }))
      } else if (u.pathname === '/api/redir') {
        res.writeHead(302, { location: '/api/echo?q=fromredir' })
        res.end()
      } else if (u.pathname === '/api/fail') {
        res.writeHead(404, { 'content-type': 'text/plain' })
        res.end('nope')
      } else if (u.pathname === '/api/data') {
        res.writeHead(200, { 'content-type': 'application/json', 'x-orig': 'yes' })
        res.end(JSON.stringify({ items: ['real'] }))
      } else {
        res.writeHead(200, { 'content-type': 'text/plain' })
        res.end('ok')
      }
    })
    await new Promise((resolve) => srv.listen(0, '127.0.0.1', resolve))
    const apiBase = `http://127.0.0.1:${srv.address().port}`
    const ctx = await browser.newContext({ baseURL: apiBase, extraHTTPHeaders: { 'x-ctx': 'node-ctx' } })
    try {
      await ctx.addCookies([
        { name: 'root_c', value: '1', domain: '127.0.0.1', path: '/' },
        { name: 'app_c', value: '2', domain: '127.0.0.1', path: '/app' },
        { name: 'loc_c', value: '3', domain: 'localhost', path: '/' },
      ])
      const appNames = (await ctx.cookies(`${apiBase}/app/sub`)).map((c) => c.name).sort()
      assert.deepEqual(appNames, ['app_c', 'root_c'])
      assert.deepEqual((await ctx.cookies([`${apiBase}/apple`])).map((c) => c.name), ['root_c'])
      await ctx.clearCookies({ name: /^app_/ })
      await ctx.clearCookies({ domain: 'localhost' })
      assert.deepEqual((await ctx.cookies()).map((c) => c.name), ['root_c'])
      await ctx.clearCookies()

      const p = await ctx.newPage()
      await p.goto('/idb')
      await p.evaluate(async () => {
        localStorage.setItem('lsNode', 'val')
        await new Promise((resolve, reject) => {
          const req = indexedDB.open('nodeDb', 1)
          req.onupgradeneeded = () => {
            const db = req.result
            db.createObjectStore('store', { keyPath: 'id' })
          }
          req.onsuccess = () => {
            const db = req.result
            const tx = db.transaction('store', 'readwrite')
            tx.objectStore('store').put({ id: 'k1', ts: new Date('2026-02-03T04:05:06.000Z'), secret: 'idb-ok' })
            tx.oncomplete = () => { db.close(); resolve() }
            tx.onerror = () => reject(tx.error)
          }
        })
      })
      const idbState = await ctx.storageState({ indexedDB: true })
      assert.ok(idbState.origins.some((o) => o.indexedDB?.some((d) => d.name === 'nodeDb')))

      const restoredCtx = await browser.newContext({ storageState: idbState })
      try {
        const rp = await restoredCtx.newPage()
        await rp.goto(`${apiBase}/idb`)
        const got = await rp.evaluate(() => new Promise((resolve, reject) => {
          const req = indexedDB.open('nodeDb', 1)
          req.onsuccess = () => {
            const db = req.result
            const tx = db.transaction('store', 'readonly')
            const get = tx.objectStore('store').get('k1')
            tx.oncomplete = () => {
              const r = get.result
              db.close()
              resolve({ ls: localStorage.getItem('lsNode'), secret: r?.secret, isDate: r?.ts instanceof Date })
            }
            tx.onerror = () => reject(tx.error)
          }
        }))
        assert.deepEqual(got, { ls: 'val', secret: 'idb-ok', isDate: true })
      } finally {
        await restoredCtx.close()
      }

      const login = await ctx.request.post('/api/login', { data: { hello: 1 } })
      assert.equal(login.ok(), true)
      const lj = await login.json()
      assert.equal(lj.xCtx, 'node-ctx')
      assert.match(lj.ctype, /application\/json/)
      assert.equal(await p.evaluate(() => document.cookie), 'vis=yes')
      await p.evaluate(() => { document.cookie = 'from_pg=xyz; Path=/' })

      const echo = await p.request.get('/api/echo', { params: { q: 'hi there' } })
      const ej = await echo.json()
      assert.ok(ej.cookie.includes('api_tok=node99') && ej.cookie.includes('from_pg=xyz'))
      assert.equal(ej.query, 'hi there')

      const mp = await ctx.request.post('/api/login', { multipart: { f: { name: 'hello.txt', mimeType: 'text/plain', buffer: Buffer.from('abc') } } })
      const mj = await mp.json()
      assert.match(mj.ctype, /^multipart\/form-data/)
      assert.ok(mj.body.includes('filename="hello.txt"') && mj.body.includes('abc'))

      const noRedir = await ctx.request.get('/api/redir', { maxRedirects: 0 })
      assert.equal(noRedir.status(), 302)
      await assert.rejects(() => ctx.request.get('/api/fail', { failOnStatusCode: true }), /404/)

      await p.route('**/api/data', async (route) => {
        const resp = await route.fetch()
        const json = await resp.json()
        json.items.push('patched')
        await route.fulfill({ response: resp, json })
      })
      const patched = await p.evaluate(() => fetch('/api/data').then((r) => r.json()))
      assert.deepEqual(patched.items, ['real', 'patched'])
    } finally {
      await ctx.close()
      srv.close()
    }
  }

  // PR 4C: WebSockets, HAR 1.2 record/replay/fallback/update, dedicated Web Workers
  {
    const harDir = await mkdtemp(join(tmpdir(), 'fw-node-har-'))
    const harPath = join(harDir, 'rec.har')
    const harUpdatePath = join(harDir, 'upd.har')
    let apiHits = 0
    let seq = 0
    const pr4cSrv = http.createServer((req, res) => {
      const u = new URL(req.url, 'http://127.0.0.1')
      if (u.pathname === '/') {
        res.writeHead(200, { 'content-type': 'text/html' })
        return res.end('<!doctype html><h1>pr4c-node</h1>')
      }
      if (u.pathname === '/worker.js') {
        res.writeHead(200, { 'content-type': 'application/javascript' })
        return res.end('self.workerValue = 37; self.emitLog = (m) => console.log(m);')
      }
      if (u.pathname === '/api/hello') {
        apiHits += 1
        res.writeHead(200, { 'content-type': 'application/json', 'x-src': 'node-srv' })
        return res.end(JSON.stringify({ msg: 'hello', hits: apiHits }))
      }
      if (u.pathname === '/api/seq') {
        seq += 1
        res.writeHead(200, { 'content-type': 'application/json' })
        return res.end(JSON.stringify({ n: seq }))
      }
      if (u.pathname === '/api/other') {
        res.writeHead(200, { 'content-type': 'application/json' })
        return res.end(JSON.stringify({ other: true }))
      }
      res.writeHead(200, { 'content-type': 'text/plain' })
      res.end('ok')
    })
    const makeWsFrame = (opcode, payloadBuf) => {
      const len = payloadBuf.length
      const hdr =
        len < 126
          ? Buffer.from([0x80 | opcode, len])
          : Buffer.from([0x80 | opcode, 126, (len >> 8) & 0xff, len & 0xff])
      return Buffer.concat([hdr, payloadBuf])
    }
    const wsSockets = new Set()
    pr4cSrv.on('upgrade', (req, socket) => {
      const wsNonce = req.headers['sec-websocket-key'] || ''
      const accept = createHash('sha1').update(`${wsNonce}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64')
      socket.write(
        'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n'
          + `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
      )
      wsSockets.add(socket)
      socket.on('close', () => wsSockets.delete(socket))
      let buf = Buffer.alloc(0)
      socket.on('data', (chunk) => {
        buf = Buffer.concat([buf, chunk])
        while (buf.length >= 2) {
          const opcode = buf[0] & 0x0f
          const masked = (buf[1] & 0x80) !== 0
          let plen = buf[1] & 0x7f
          let off = 2
          if (plen === 126) {
            if (buf.length < 4) return
            plen = buf.readUInt16BE(2)
            off = 4
          }
          const maskLen = masked ? 4 : 0
          if (buf.length < off + maskLen + plen) return
          const payload = Buffer.from(buf.subarray(off + maskLen, off + maskLen + plen))
          if (masked) {
            const m = buf.subarray(off, off + 4)
            for (let i = 0; i < plen; i++) payload[i] ^= m[i & 3]
          }
          buf = buf.subarray(off + maskLen + plen)
          if (opcode === 0x8) {
            socket.end(makeWsFrame(0x8, Buffer.alloc(0)))
            return
          }
          if (opcode === 0x1) {
            socket.write(makeWsFrame(0x1, Buffer.from(`echo:${payload.toString('utf8')}`)))
          } else if (opcode === 0x2) {
            socket.write(makeWsFrame(0x2, Buffer.concat([Buffer.from([0xfe]), payload])))
          } else if (opcode === 0x9) {
            socket.write(makeWsFrame(0xa, payload))
          }
        }
      })
    })
    await new Promise((resolve) => pr4cSrv.listen(0, '127.0.0.1', resolve))
    const port = pr4cSrv.address().port
    const pr4cBase = `http://127.0.0.1:${port}`
    const pr4cWs = `ws://127.0.0.1:${port}`
    try {
      const recCtx = await browser.newContext({
        baseURL: pr4cBase,
        recordHar: { path: harPath, mode: 'minimal', urlFilter: /\/api\// },
      })
      try {
        const p = await recCtx.newPage()
        await p.goto('/')

        // WebSockets
        const wsPromise = p.waitForEvent('websocket')
        await p.evaluate(
          (u) =>
            new Promise((resolve, reject) => {
              const ws = new WebSocket(u)
              ws.binaryType = 'arraybuffer'
              window.__ws = ws
              ws.onopen = () => resolve()
              ws.onerror = () => reject(new Error('ws open failed'))
            }),
          pr4cWs,
        )
        const ws = await wsPromise
        assert.ok(ws.url().startsWith(pr4cWs))
        const sentTextP = ws.waitForEvent('framesent', (f) => f.payload === 'hi-node')
        const sentBinP = ws.waitForEvent(
          'framesent',
          (f) => Buffer.isBuffer(f.payload) && Buffer.compare(f.payload, Buffer.from([7, 8])) === 0,
        )
        const recvTextP = ws.waitForEvent('framereceived', (f) => f.payload === 'echo:hi-node')
        const recvBinP = ws.waitForEvent(
          'framereceived',
          (f) => Buffer.isBuffer(f.payload) && Buffer.compare(f.payload, Buffer.from([0xfe, 7, 8])) === 0,
        )
        const wsCloseP = ws.waitForEvent('close')
        const replies = await p.evaluate(async () => {
          const ws = window.__ws
          const out = []
          ws.onmessage = (e) => {
            out.push(typeof e.data === 'string' ? e.data : Array.from(new Uint8Array(e.data)))
          }
          ws.send('hi-node')
          ws.send(new Uint8Array([7, 8]))
          const deadline = Date.now() + 5000
          while (out.length < 2 && Date.now() < deadline) await new Promise((r) => setTimeout(r, 15))
          ws.close(1000, 'bye')
          await new Promise((r) => {
            ws.onclose = r
            setTimeout(r, 1000)
          })
          return out
        })
        assert.deepEqual(replies, ['echo:hi-node', [0xfe, 7, 8]])
        await Promise.all([sentTextP, sentBinP, recvTextP, recvBinP, wsCloseP])
        assert.equal(ws.isClosed(), true)

        // Dedicated Web Workers
        const workerPromise = p.waitForEvent('worker')
        await p.evaluate(() => {
          window.__w = new Worker('/worker.js')
        })
        const worker = await workerPromise
        assert.ok(worker.url().endsWith('/worker.js'))
        assert.ok(p.workers().includes(worker))
        let wVal = null
        for (let i = 0; i < 40; i++) {
          wVal = await worker.evaluate((n) => (typeof self.workerValue === 'number' ? self.workerValue + n : null), 5)
          if (wVal === 42) break
          await new Promise((r) => setTimeout(r, 25))
        }
        assert.equal(wVal, 42)
        const wConsoleP = worker.waitForEvent('console', (m) => m.text === 'worker-node')
        await worker.evaluate(() => self.emitLog('worker-node'))
        const wMsg = await wConsoleP
        assert.equal(wMsg.text, 'worker-node')
        const wCloseP = worker.waitForEvent('close')
        await p.evaluate(() => window.__w.terminate())
        await wCloseP
        assert.equal(p.workers().length, 0)

        // Traffic recorded into HAR
        const hello = await p.evaluate(() => fetch('/api/hello').then((r) => r.json()))
        assert.equal(hello.msg, 'hello')
        const s1 = await p.evaluate(() => fetch('/api/seq').then((r) => r.json()))
        const s2 = await p.evaluate(() => fetch('/api/seq').then((r) => r.json()))
        assert.deepEqual([s1.n, s2.n], [1, 2])
      } finally {
        await recCtx.close()
      }

      const harJson = JSON.parse(await readFile(harPath, 'utf8'))
      assert.equal(harJson.log.version, '1.2')
      assert.ok(harJson.log.entries.length >= 3)
      assert.ok(harJson.log.entries.every((e) => /\/api\//.test(e.request.url)))

      apiHits = 0
      const repCtx = await browser.newContext({ baseURL: pr4cBase })
      try {
        await repCtx.routeFromHAR(harPath, { url: '**/api/*', notFound: 'abort' })
        const rp = await repCtx.newPage()
        await rp.goto('/')
        const hello = await rp.evaluate(() => fetch('/api/hello').then((r) => r.json()))
        assert.equal(hello.msg, 'hello')
        const s1 = await rp.evaluate(() => fetch('/api/seq').then((r) => r.json()))
        const s2 = await rp.evaluate(() => fetch('/api/seq').then((r) => r.json()))
        assert.deepEqual([s1.n, s2.n], [1, 2])
        assert.equal(apiHits, 0)
        const aborted = await rp.evaluate(() => fetch('/api/other').then(() => false).catch(() => true))
        assert.equal(aborted, true)
      } finally {
        await repCtx.close()
      }

      const fbCtx = await browser.newContext({ baseURL: pr4cBase })
      try {
        const fp = await fbCtx.newPage()
        await fp.goto('/')
        await fp.route('**/api/other', (route) => route.fulfill({ json: { mockedFallback: true } }))
        await fp.routeFromHAR(harPath, { url: '**/api/*', notFound: 'fallback' })
        const rHello = await fp.evaluate(() => fetch('/api/hello').then((r) => r.json()))
        assert.equal(rHello.msg, 'hello')
        const rOther = await fp.evaluate(() => fetch('/api/other').then((r) => r.json()))
        assert.deepEqual(rOther, { mockedFallback: true })
        assert.equal(apiHits, 0)
      } finally {
        await fbCtx.close()
      }

      const updCtx = await browser.newContext({ baseURL: pr4cBase })
      try {
        await updCtx.routeFromHAR(harUpdatePath, { url: '**/api/*', update: true })
        const up = await updCtx.newPage()
        await up.goto('/')
        const rOther = await up.evaluate(() => fetch('/api/other').then((r) => r.json()))
        assert.deepEqual(rOther, { other: true })
      } finally {
        await updCtx.close()
      }
      const updHar = JSON.parse(await readFile(harUpdatePath, 'utf8'))
      assert.ok(updHar.log.entries.some((e) => e.request.url.endsWith('/api/other')))
    } finally {
      for (const s of wsSockets) s.destroy()
      pr4cSrv.close()
      await rm(harDir, { recursive: true, force: true })
    }
  }

  // Phase 3 PR 5A: custom args, ignoreDefaultArgs, unknown channel error, prewarmContexts isolation, resource presets
  {
    await assert.rejects(() => fluxwright.chromium.launch({ channel: 'bogus-browser' }), /unsupported browser channel/)
    await assert.rejects(() => fluxwright.chromium.launch({ prewarmContexts: -1 }), TypeError)
    const seenPaths = []
    const resSrv = http.createServer((req, res) => {
      const u = new URL(req.url, 'http://127.0.0.1')
      seenPaths.push(u.pathname)
      if (u.pathname === '/') {
        res.writeHead(200, { 'content-type': 'text/html' })
        return res.end(
          '<!doctype html><head><link rel="stylesheet" href="/style.css"></head>' +
            '<body><img src="/img.png"><script src="/app.js"></script><script src="/tracker.js"></script>' +
            '<style>@font-face{font-family:F;src:url(/font.woff2)}body{font-family:F}</style><p>hi</p></body>',
        )
      }
      if (u.pathname === '/style.css') {
        res.writeHead(200, { 'content-type': 'text/css' })
        return res.end('p { color: rgb(1, 2, 3); }')
      }
      if (u.pathname === '/app.js') {
        res.writeHead(200, { 'content-type': 'application/javascript' })
        return res.end('window.__appLoaded = true;')
      }
      if (u.pathname === '/tracker.js') {
        res.writeHead(200, { 'content-type': 'application/javascript' })
        return res.end('window.__trackerLoaded = true;')
      }
      if (u.pathname === '/img.png') {
        res.writeHead(200, { 'content-type': 'image/png' })
        return res.end(Buffer.alloc(10))
      }
      if (u.pathname === '/font.woff2') {
        res.writeHead(200, { 'content-type': 'font/woff2' })
        return res.end(Buffer.alloc(10))
      }
      res.writeHead(200, { 'content-type': 'text/plain' })
      res.end('ok')
    })
    await new Promise((resolve) => resSrv.listen(0, '127.0.0.1', resolve))
    const resBase = `http://127.0.0.1:${resSrv.address().port}`
    const pwBrowser = await fluxwright.chromium.launch({
      maxBrowsers: 1,
      prewarmContexts: 2,
      args: ['--user-agent=FluxwrightNodeArg/1.0'],
      ignoreDefaultArgs: ['--mute-audio'],
    })
    try {
      const p1 = await pwBrowser.newPage({ resourcePreset: 'scraping' })
      assert.equal(await p1.evaluate(() => navigator.userAgent), 'FluxwrightNodeArg/1.0')
      await p1.goto(resBase)
      await p1.evaluate(() => {
        document.cookie = 'leak=1; Path=/'
        localStorage.setItem('leak_key', 'yes')
      })
      assert.equal(await p1.evaluate(() => window.__appLoaded === true), true)
      assert.ok(!seenPaths.includes('/img.png'))
      assert.ok(seenPaths.includes('/style.css'))
      assert.ok(seenPaths.includes('/app.js'))
      await p1.close()

      seenPaths.length = 0
      const p2 = await pwBrowser.newPage({ resourcePreset: 'minimal', blockUrlPatterns: ['*tracker.js'] })
      await p2.goto(resBase)
      assert.deepEqual(
        await p2.evaluate(() => [document.cookie, localStorage.getItem('leak_key'), window.__appLoaded === true, window.__trackerLoaded === true]),
        ['', null, true, false],
      )
      assert.ok(!seenPaths.includes('/style.css'))
      assert.ok(!seenPaths.includes('/tracker.js'))
      await p2.close()

      seenPaths.length = 0
      const p3 = await pwBrowser.newPage({ blockResources: ['script'] })
      await p3.goto(resBase)
      assert.equal(await p3.evaluate(() => window.__appLoaded === true), false)
      assert.ok(!seenPaths.includes('/app.js'))
      await p3.close()
    } finally {
      await pwBrowser.close()
      resSrv.close()
    }
  }

  // AI snapshots (@eN refs) and clean Markdown extraction
  {
    const aiSrv = http.createServer((req, res) => {
      res.setHeader('Content-Type', 'text/html')
      if (req.url === '/next') {
        return res.end('<title>Next</title><p>Done</p>')
      }
      res.end(
        '<title>AI Fixture</title>' +
          '<header><a href="/about">Nav</a></header>' +
          '<main>' +
          '<h1>Welcome</h1>' +
          '<p>Read <a href="https://example.com/docs">the docs</a> carefully.</p>' +
          '<span style="display:none">SECRET HIDDEN TEXT</span>' +
          '<ul><li>Alpha</li><li>Beta</li></ul>' +
          '<pre><code>const x = 1;</code></pre>' +
          '<table><tr><th>K</th><th>V</th></tr><tr><td>a</td><td>1</td></tr></table>' +
          '<label for="name">Name</label><input id="name" aria-label="Name" type="text">' +
          '<button onclick="document.getElementById(\'out\').textContent = document.getElementById(\'name\').value">Apply</button>' +
          '<div id="out"></div>' +
          '</main>',
      )
    })
    await new Promise((resolve) => aiSrv.listen(0, '127.0.0.1', resolve))
    const aiBase = `http://127.0.0.1:${aiSrv.address().port}`
    const aiPage = await browser.newPage()
    try {
      await aiPage.goto(aiBase)
      const fullSnap = await aiPage.ariaSnapshot()
      assert.match(fullSnap.tree, /heading "Welcome"/)
      assert.ok(fullSnap.refs.some((r) => r.role === 'button' && r.name === 'Apply'))

      const snap = await aiPage.ariaSnapshot({ interactiveOnly: true })
      const inputRef = snap.refs.find((r) => r.role === 'textbox' && r.name === 'Name')
      const btnRef = snap.refs.find((r) => r.role === 'button' && r.name === 'Apply')
      assert.ok(inputRef && btnRef)
      await aiPage.fill(`@${inputRef.id}`, 'hello')
      await aiPage.click(`ref=${btnRef.id}`)
      assert.equal(await aiPage.textContent('#out'), 'hello')

      const md = await aiPage.markdown({ mainContentOnly: true })
      assert.match(md, /^# Welcome/m)
      assert.match(md, /\[the docs\]\(https:\/\/example\.com\/docs\)/)
      assert.match(md, /- Alpha/)
      assert.match(md, /```\nconst x = 1;\n```/)
      assert.match(md, /\| K \| V \|/)
      assert.ok(!md.includes('SECRET HIDDEN TEXT'))
      assert.ok(!md.includes('[Nav]'))

      await aiPage.goto(`${aiBase}/next`)
      await assert.rejects(() => aiPage.click(`@${btnRef.id}`), /unknown element ref|stale/)
    } finally {
      await aiPage.close()
      aiSrv.close()
    }
  }

  // Diff-only ariaSnapshot, Set-of-Marks annotate, and screenshot masking (incl. Shadow DOM) on a fresh page.
  {
    const diffPage = await browser.newPage()
    try {
      await assert.rejects(() => diffPage.ariaSnapshot({ diff: 'yes' }), TypeError)
      await diffPage.setContent(
        '<!doctype html><style>' +
          'body{margin:0;background:#fff}' +
          '#wrap{position:fixed;left:0;top:0;width:200px;height:80px;background:#fff}' +
          '#secret{position:fixed;left:10px;top:10px;width:60px;height:60px;background:#fff}' +
          '#go{position:fixed;left:10px;top:90px;width:90px;height:32px}' +
          '#caret-box{position:fixed;left:110px;top:90px;width:120px;height:32px}' +
          '@keyframes pulse{0%{opacity:1}50%{opacity:.2}100%{opacity:1}}' +
          '#anim{position:fixed;left:220px;top:10px;width:40px;height:40px;background:#00f;animation:pulse 1s infinite}' +
          '</style>' +
          '<div id="wrap"><div id="secret">Secret</div><div id="host"></div></div>' +
          '<button id="go" onclick="document.getElementById(\'out\').innerHTML=\'<button id=added>Added</button>\'">Go</button>' +
          '<input id="caret-box" aria-label="Caret" type="text">' +
          '<div id="anim"></div>' +
          '<div id="out">idle</div>' +
          '<script>' +
          'const sh=document.getElementById("host").attachShadow({mode:"open"});' +
          'sh.innerHTML="<div id=shadow-secret style=\'position:fixed;left:100px;top:10px;width:60px;height:60px;background:#fff\'>S</div>";' +
          '</script>',
      )
      const d0 = await diffPage.ariaSnapshot({ diff: true })
      assert.equal(d0.isDiff, false)
      assert.equal(d0.changed, true)
      assert.match(d0.tree, /button "Go"/)

      const d1 = await diffPage.ariaSnapshot({ diff: true })
      assert.equal(d1.isDiff, true)
      assert.equal(d1.changed, false)
      assert.equal(d1.tree, '(no changes)')
      const goRef = d1.refs.find((r) => r.role === 'button' && r.name === 'Go')
      assert.ok(goRef)

      const somPng = await diffPage.screenshot({ annotate: true })
      assert.equal(somPng.subarray(0, 4).toString('latin1'), '\x89PNG')
      assert.equal(await diffPage.evaluate(() => document.getElementById('__fluxwright_som_overlay') === null), true)
      const somJpeg = await diffPage.screenshot({ annotate: true, type: 'jpeg', quality: 80 })
      assert.equal(somJpeg[0], 0xff)
      assert.equal(somJpeg[1], 0xd8)

      // annotate: true must not overwrite diff baseline cache.
      const stillUnchanged = await diffPage.ariaSnapshot({ diff: true })
      assert.equal(stillUnchanged.isDiff, true)
      assert.equal(stillUnchanged.changed, false)
      assert.equal(stillUnchanged.tree, '(no changes)')

      await diffPage.click(`@${goRef.id}`)
      const d2 = await diffPage.ariaSnapshot({ diff: true })
      assert.equal(d2.isDiff, true)
      assert.equal(d2.changed, true)
      assert.ok(d2.tree.includes('+ '))
      assert.match(d2.tree, /button "Added"/)

      const resetSnap = await diffPage.ariaSnapshot()
      assert.equal(resetSnap.isDiff, false)
      assert.equal(resetSnap.changed, true)

      await diffPage.click('#caret-box')
      const maskedPng = await diffPage.screenshot({
        mask: [diffPage.locator('#secret'), '#shadow-secret'],
        maskColor: '#00FF00',
        animations: 'disabled',
        caret: 'hide',
      })
      const dec = decodePng(maskedPng)
      const px = (x, y) => {
        const off = (y * dec.width + x) * 4
        return Array.from(dec.data.subarray(off, off + 4))
      }
      assert.deepEqual(px(40, 40), [0, 255, 0, 255])
      assert.deepEqual(px(130, 40), [0, 255, 0, 255])
      assert.equal(
        await diffPage.evaluate(() => !document.getElementById('__fluxwright_mask_overlay') && !document.getElementById('__fluxwright_screenshot_style')),
        true,
      )

      const elemMasked = await diffPage.locator('#wrap').screenshot({ mask: ['#secret'], maskColor: '#FF0000' })
      const elemDec = decodePng(elemMasked)
      const elemOff = (40 * elemDec.width + 40) * 4
      assert.deepEqual(Array.from(elemDec.data.subarray(elemOff, elemOff + 4)), [255, 0, 0, 255])
      assert.equal(await diffPage.evaluate(() => document.getElementById('__fluxwright_mask_overlay') === null), true)

      const otherPage = await browser.newPage()
      try {
        await assert.rejects(() => diffPage.screenshot({ mask: [otherPage.locator('#secret')] }), TypeError)
      } finally {
        await otherPage.close()
      }

      await diffPage.goto('about:blank')
      const blankSom = await diffPage.screenshot({ annotate: true })
      assert.equal(blankSom.subarray(0, 4).toString('latin1'), '\x89PNG')
    } finally {
      await diffPage.close()
    }
  }

  // page.clock: install, pauseAt, runFor, fastForward, setFixedTime, setSystemTime, resume, navigation persistence.
  {
    const clockSrv = http.createServer((req, res) => {
      if (req.url === '/clock-a') {
        res.writeHead(200, { 'content-type': 'text/html' })
        res.end(`<!doctype html><script>
          window.__bootNow = Date.now()
          window.__bootIso = new Date().toISOString()
          window.__events = []
          window.__ivCount = 0
          window.__rafCount = 0
          setTimeout(() => {
            window.__events.push('t1500:' + Date.now())
            Promise.resolve().then(() => {
              setTimeout(() => window.__events.push('micro:' + Date.now()), 100)
            })
          }, 1500)
          const iv = setInterval(() => {
            window.__ivCount++
            window.__events.push('iv:' + window.__ivCount + ':' + Date.now())
          }, 1000)
          window.__stopIv = () => clearInterval(iv)
          requestAnimationFrame(() => { window.__rafCount++ })
        </script>`)
        return
      }
      res.writeHead(200, { 'content-type': 'text/html' })
      res.end(`<!doctype html><script>
        window.__step2Now = Date.now()
        window.__step2Instance = new Date() instanceof Date
        window.__step2DateFn = typeof Date()
        window.__step2Utc = Date.UTC(2024, 1, 2, 8, 0, 0)
        window.__step2Parse = Date.parse('2024-02-02T08:00:00Z')
      </script>`)
    })
    await new Promise((r) => clockSrv.listen(0, '127.0.0.1', r))
    const clockBase = `http://127.0.0.1:${clockSrv.address().port}`
    const cp = await browser.newPage()
    try {
      const t0 = Date.parse('2024-02-02T08:00:00Z')
      await cp.clock.install({ time: new Date(t0) })
      await cp.clock.pauseAt(t0)
      await cp.goto(`${clockBase}/clock-a`)
      assert.equal(await cp.evaluate(() => window.__bootNow), t0)
      assert.equal(await cp.evaluate(() => window.__bootIso), '2024-02-02T08:00:00.000Z')

      const perf0 = await cp.evaluate(() => performance.now())
      await new Promise((r) => setTimeout(r, 80))
      assert.equal(await cp.evaluate(() => Date.now()), t0)
      assert.equal(await cp.evaluate(() => performance.now()), perf0)
      assert.equal(await cp.evaluate(() => window.__ivCount), 0)

      await cp.clock.runFor(2500)
      const perf1 = await cp.evaluate(() => performance.now())
      assert.ok(Math.abs(perf1 - perf0 - 2500) < 1e-6)
      assert.equal(await cp.evaluate(() => Date.now()), t0 + 2500)
      assert.equal(await cp.evaluate(() => window.__ivCount), 2)
      assert.equal(await cp.evaluate(() => window.__rafCount), 1)
      assert.deepEqual(await cp.evaluate(() => window.__events), [
        `iv:1:${t0 + 1000}`,
        `t1500:${t0 + 1500}`,
        `micro:${t0 + 1600}`,
        `iv:2:${t0 + 2000}`,
      ])

      await cp.clock.fastForward('05:00')
      assert.equal(await cp.evaluate(() => window.__ivCount), 3)
      assert.equal(await cp.evaluate(() => Date.now()), t0 + 2500 + 300_000)
      await cp.clock.fastForward('00:00:10')
      assert.equal(await cp.evaluate(() => window.__ivCount), 4)
      assert.equal(await cp.evaluate(() => Date.now()), t0 + 2500 + 310_000)
      await cp.evaluate(() => window.__stopIv())

      await cp.goto(`${clockBase}/clock-b`)
      assert.equal(await cp.evaluate(() => window.__step2Now), t0 + 2500 + 310_000)
      assert.equal(await cp.evaluate(() => window.__step2Instance), true)
      assert.equal(await cp.evaluate(() => window.__step2DateFn), 'string')
      assert.equal(await cp.evaluate(() => window.__step2Utc), t0)
      assert.equal(await cp.evaluate(() => window.__step2Parse), t0)

      const fixedTs = Date.parse('2025-01-01T00:00:00Z')
      await cp.clock.setFixedTime('2025-01-01T00:00:00Z')
      await cp.evaluate(() => {
        window.__fixedFired = 0
        setTimeout(() => {
          window.__fixedFired = Date.now()
        }, 200)
      })
      const pBeforeFixed = await cp.evaluate(() => performance.now())
      await cp.clock.runFor(500)
      const pAfterFixed = await cp.evaluate(() => performance.now())
      assert.ok(Math.abs(pAfterFixed - pBeforeFixed - 500) < 1e-6)
      assert.equal(await cp.evaluate(() => Date.now()), fixedTs)
      assert.equal(await cp.evaluate(() => window.__fixedFired), fixedTs)

      await cp.clock.setSystemTime(t0)
      assert.equal(await cp.evaluate(() => Date.now()), t0)

      await cp.evaluate(() => {
        window.__resumedFired = false
        setTimeout(() => {
          window.__resumedFired = true
        }, 40)
      })
      await cp.clock.resume()
      for (let i = 0; i < 40 && !(await cp.evaluate(() => window.__resumedFired)); i++) {
        await new Promise((r) => setTimeout(r, 20))
      }
      assert.equal(await cp.evaluate(() => window.__resumedFired), true)
      assert.ok((await cp.evaluate(() => Date.now())) > t0)

      await assert.rejects(() => cp.clock.fastForward(-1), TypeError)
      await assert.rejects(() => cp.clock.runFor(Number.NaN), TypeError)
      await assert.rejects(() => cp.clock.fastForward('01:60'), /ClockTicks/)
      await assert.rejects(() => cp.clock.install({ time: new Date('bad') }), TypeError)
      await assert.rejects(() => cp.clock.pauseAt(''), TypeError)
      await assert.rejects(() => cp.clock.setFixedTime('2024-02-30T00:00:00Z'), /ClockTime/)
    } finally {
      await cp.close()
      clockSrv.close()
    }
  }

  // page.route / context.route { times } and multi-handler route.fallback(overrides).
  {
    const routeSrv = http.createServer((req, res) => {
      const chunks = []
      req.on('data', (c) => chunks.push(c))
      req.on('end', () => {
        res.writeHead(200, { 'content-type': 'application/json' })
        res.end(
          JSON.stringify({
            method: req.method,
            url: req.url,
            headers: req.headers,
            body: Buffer.concat(chunks).toString('utf8'),
          }),
        )
      })
    })
    await new Promise((r) => routeSrv.listen(0, '127.0.0.1', r))
    const routeBase = `http://127.0.0.1:${routeSrv.address().port}`
    const rCtx = await browser.newContext({ baseURL: routeBase })
    try {
      await assert.rejects(() => rCtx.route('**/echo/*', () => {}, { times: -1 }), TypeError)
      const ctxLog = []
      await rCtx.route(
        '**/echo/*',
        async (route, request) => {
          ctxLog.push({
            url: request.url(),
            method: request.method(),
            xOuter: request.headers()['x-outer'],
            xMid: request.headers()['x-mid'],
            postData: request.postData(),
          })
          await route.fallback()
        },
        { times: 2 },
      )
      const rp = await rCtx.newPage()
      await assert.rejects(() => rp.route('**/echo/*', () => {}, { times: 1.5 }), TypeError)
      await rp.goto('/route-home')

      // Older page route inspects overrides from newer route and adds x-mid + new url before falling back to context route.
      await rp.route('**/echo/*', async (route, request) => {
        if (request.headers()['x-outer']) {
          assert.equal(request.method(), 'POST')
          assert.equal(request.postData(), 'from-outer')
          await route.fallback({
            url: `${routeBase}/echo/final?step=2`,
            headers: { ...request.headers(), 'X-Mid': 'from-mid' },
          })
          return
        }
        if (request.url().includes('/echo/after')) {
          await route.fallback()
          return
        }
        await route.fulfill({ json: { directToOlder: true } })
      })

      let doubleFallbackErr = null
      let zeroHits = 0
      await rp.route(
        '**/echo/start',
        async (route, request) => {
          await route.fallback({
            method: 'POST',
            headers: { ...request.headers(), 'X-Outer': 'from-outer' },
            postData: Buffer.from('from-outer'),
          })
          await route.fallback().catch((e) => (doubleFallbackErr = e.message))
        },
        { times: 2 },
      )
      await rp.route(
        '**/echo/start',
        (route) => {
          zeroHits++
          return route.fulfill({ body: 'never' })
        },
        { times: 0 },
      )

      for (let i = 0; i < 2; i++) {
        const res = await rp.evaluate(() => fetch('/echo/start').then((r) => r.json()))
        assert.equal(res.method, 'POST')
        assert.equal(res.url, '/echo/final?step=2')
        assert.equal(res.headers['x-outer'], 'from-outer')
        assert.equal(res.headers['x-mid'], 'from-mid')
        assert.equal(res.body, 'from-outer')
      }
      assert.match(doubleFallbackErr, /already handled/)
      assert.equal(zeroHits, 0)
      assert.equal(ctxLog.length, 2)
      assert.equal(ctxLog[0].xOuter, 'from-outer')
      assert.equal(ctxLog[0].xMid, 'from-mid')
      assert.equal(ctxLog[0].postData, 'from-outer')

      // 3rd request: outer times:2 handler is gone, so /echo/start hits older page handler directly.
      const third = await rp.evaluate(() => fetch('/echo/start').then((r) => r.json()))
      assert.deepEqual(third, { directToOlder: true })
      // Context times:2 handler is also exhausted (mid falls back on /echo/after and reaches server directly).
      const fourth = await rp.evaluate(() => fetch('/echo/after').then((r) => r.json()))
      assert.equal(fourth.url, '/echo/after')
      assert.equal(fourth.method, 'GET')
      assert.equal(ctxLog.length, 2)
      await rp.close()
    } finally {
      await rCtx.close()
      routeSrv.close()
    }
  }

  // Left open on purpose: Node drops it at exit, outside the engine's runtime (0.2.1 crashed there).
  await browser.newPage()
} finally {
  await browser.close()
  server.close()
}
console.log('smoke ok')
