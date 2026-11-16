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
  assert.throws(() => page.locator('p', { hasText: 'x' }), TypeError)
  await assert.rejects(page.click('p', { button: 'right' }), TypeError)
  await assert.rejects(page.screenshot({ path: join(tmpdir(), 'fluxwright.gif') }), TypeError)
  await assert.rejects(page.screenshot({ path: join(tmpdir(), 'fluxwright.webp') }), TypeError)
  await assert.rejects(page.screenshot({ type: 'webp' }), TypeError)
  await assert.rejects(page.screenshot({ animations: 'disabled' }), TypeError)
  await assert.rejects(page.screenshot({ mask: [] }), TypeError)
  await assert.rejects(page.screenshot({ clip: { x: 0, y: 0, width: 10, height: 10, extra: 1 } }), TypeError)
  await assert.rejects(page.screenshot({ type: 'png', quality: 50 }), /JPEG only/)
  await assert.rejects(page.pdf({ format: 'A9' }), /unknown format/)
  await assert.rejects(page.pdf({ landscpe: true }), TypeError)
  await assert.rejects(page.pdf({ margin: { bogus: 10 } }), TypeError)
  await assert.rejects(browser.newPage({ recordVideo: { dir: '.' } }), TypeError)
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
  const pending = rv.waitForDownload({ timeout: 10000 })
  await rv.evaluate(() => {
    const a = document.createElement('a')
    a.href = '/report'
    document.body.append(a)
    a.click()
  })
  assert.equal((await pending).suggestedFilename(), 'report.csv')
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
  await assert.rejects(context.newPage(), /holds one page/)
  await context.close()
  assert.ok(inContext.isClosed())
  assert.deepEqual(context.pages(), [])
  await assert.rejects(context.newPage(), /context is closed/)

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

  // Left open on purpose: Node drops it at exit, outside the engine's runtime (0.2.1 crashed there).
  await browser.newPage()
} finally {
  await browser.close()
  server.close()
}
console.log('smoke ok')
