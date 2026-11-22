// Compile-only check of the published typings: `npm run test:types`.
import fluxwright, {
  chromium,
  expect,
  type LocatorAssertions,
  type PageAssertions,
  type RecordVideoOptions,
  type StorageState,
  type ToHaveScreenshotOptions,
  type Tracing,
  type Video,
} from '../addon'

export async function typed(): Promise<void> {
  const browser = await chromium.launch({ maxBrowsers: 2 })
  const page = await browser.newPage()
  const title: string = await page.evaluate(() => document.title)
  const sum: number = await page.evaluate(({ a, b }) => a + b, { a: 1, b: 2 })
  const upper: string = await page.evaluate(async (s: string) => s.toUpperCase(), 'x')
  const expression: unknown = await page.evaluate('1 + 1')
  // @ts-expect-error the result of `() => 1` is a number
  const wrong: string = await page.evaluate(() => 1)
  const png: Buffer = await page.screenshot({ fullPage: true })
  const jpeg: Buffer = await page.screenshot({ type: 'jpeg', quality: 80, clip: { x: 0, y: 0, width: 10, height: 10 } })
  const pdf: Buffer = await page.pdf({ format: 'A4', margin: { top: '1cm', left: 20 }, printBackground: true, path: 'p.pdf' })
  const snap = await page.ariaSnapshot()
  const snapInteractive = await page.ariaSnapshot({ interactiveOnly: true, compact: true, maxDepth: 4, maxNodes: 500 })
  const snapCheck: [string, string, string, string, boolean] = [
    snap.tree,
    snapInteractive.refs[0].id,
    snapInteractive.refs[0].role,
    snapInteractive.refs[0].name,
    snap.truncated,
  ]
  const md1: string = await page.markdown()
  const md2: string = await page.markdown({ selector: 'main', mainContentOnly: true, includeLinks: false, includeImages: true })
  // @ts-expect-error webp is not a screenshot type
  await page.screenshot({ type: 'webp' })
  await fluxwright.chromium.launch()
  void [title, sum, upper, expression, wrong, png, jpeg, pdf, snapCheck, md1, md2]

  const state: StorageState = await page.storageState({ path: 'state.json' })
  await browser.newPage({ storageState: state, locale: 'de-DE', colorScheme: 'dark', permissions: ['geolocation'] })
  await browser.newPage({ storageState: 'state.json', geolocation: { latitude: 1, longitude: 2 } })
  // @ts-expect-error not a color scheme
  await browser.newPage({ colorScheme: 'purple' })

  const item = page.getByRole('listitem').filter({ hasText: 'x' }).nth(1).getByTestId('price')
  const box: { x: number; width: number } | null = await item.boundingBox()
  const length: number = await item.evaluate((el: { textContent: string }, n: number) => el.textContent.length + n, 1)
  const shot: Buffer = await page.getByLabel('Email').screenshot({ path: 'el.png' })
  await page.goto('https://example.com', { waitUntil: 'commit', timeout: 5_000 })
  await page.click('button', { timeout: 500 })
  await page.getByLabel('Email').fill('a@b.c', { timeout: 0 })
  await item.waitFor({ state: 'visible', timeout: 100 })
  await item.click({ button: 'right', clickCount: 2, modifiers: ['ControlOrMeta'], position: { x: 1, y: 2 }, force: true, trial: true, timeout: 100 })
  await item.dblclick({ button: 'left', delay: 5 })
  await item.hover({ modifiers: ['Shift'], trial: true })
  await item.focus({ timeout: 50 })
  await item.blur({ timeout: 50 })
  await item.scrollIntoViewIfNeeded({ timeout: 50 })
  await item.dragTo(page.locator('#dst'), { force: true, timeout: 100 })
  await page.dragAndDrop('#src', '#dst', { timeout: 100 })
  await item.press('Enter', { delay: 5, timeout: 100 })
  await item.pressSequentially('abc', { delay: 5 })
  await item.type('abc')
  await item.clear({ force: true })
  await item.check({ force: true, timeout: 50 })
  await item.uncheck({ trial: true })
  await item.setChecked(true)
  const selected: string[] = await item.selectOption(['a', { label: 'Two' }, { index: 1 }], { force: true })
  await item.setInputFiles([{ name: 'a.txt', mimeType: 'text/plain', buffer: Buffer.from('hi') }])
  await item.tap({ force: true })
  await page.mouse.move(1, 2, { steps: 3 })
  await page.mouse.down({ button: 'left', clickCount: 1 })
  await page.mouse.up({ button: 'left' })
  await page.mouse.click(1, 2, { delay: 5 })
  await page.mouse.dblclick(1, 2)
  await page.mouse.wheel(0, 100)
  await page.keyboard.down('Shift')
  await page.keyboard.up('Shift')
  await page.keyboard.press('Enter', { delay: 5 })
  await page.keyboard.type('hi')
  await page.keyboard.insertText('世')
  await page.touchscreen.tap(5, 5)
  page.on('filechooser', (fc) => void fc.setFiles([]))
  const chooser = await page.waitForFileChooser({ timeout: 100 })
  void [selected, chooser.isMultiple()]
  // @ts-expect-error invalid mouse button
  await item.click({ button: 'side' })
  page.setDefaultTimeout(2_000)
  page.setDefaultNavigationTimeout(20_000)
  const errors: Error[] = await page.pageErrors()
  const logs: { type: string; text: string }[] = await page.consoleMessages()
  void [box, length, shot, errors, logs, page.getByPlaceholder('city').first().last(), await page.screenshot({ path: 'p.png' })]

  await page.route('**/api/*', (route, request) => route.fulfill({ json: { ok: request.method() === 'GET' } }))
  await page.route(/\.png$/, (route) => route.abort('blockedbyclient'))
  await page.route((url) => url.hostname === 'example.com', (route) => route.continue({ headers: { 'x-a': '1' } }))
  await page.unroute('**/api/*')
  page.on('console', (m) => void m.text.length).on('pageerror', (e: Error) => void e.message)
  const download = await page.waitForDownload({ timeout: 5000 })
  const name: string = download.suggestedFilename()
  await download.saveAs(`out/${name}`)
  // @ts-expect-error not an event Fluxwright emits
  page.on('crash', () => {})
  page.on('websocket', (ws) => {
    void ws.url()
    void ws.isClosed()
    ws.on('framesent', ({ payload }) => void payload)
    ws.on('framereceived', ({ payload }) => void payload)
    ws.on('socketerror', (err) => void err)
    ws.on('close', (s) => void s.isClosed())
  })
  page.on('worker', (w) => {
    void w.url()
    void w.evaluate((n: number) => n + 1, 1)
    w.on('console', (m) => void m.text)
    w.on('close', () => {})
  })
  const _workers = page.workers()
  const _wsPromise = page.waitForEvent('websocket')
  const _wkPromise = page.waitForEvent('worker', (w) => w.url().includes('worker'))
  void [_workers.length, _wsPromise, _wkPromise]
  page.on('popup', (p) => void p.url())
  const pop: Promise<typeof page> = page.waitForPopup({ predicate: (p) => p.url().includes('popup'), timeout: 1000 })
  const openedBy = await page.opener()
  await page.bringToFront()
  const mainFr = page.mainFrame()
  const allFr = page.frames()
  const byName = page.frame('f')
  const byUrl = page.frame({ url: /iframe/ })
  if (byName) {
    await byName.goto('https://example.com', { waitUntil: 'domcontentloaded' })
    const ft: string = await byName.title()
    const fc: string = await byName.content()
    const fv: number = await byName.evaluate((n: number) => n + 1, 2)
    const fd: boolean = byName.isDetached()
    const fp = byName.parentFrame()
    const fkids = byName.childFrames()
    const fchild = byName.locator('#btn').or(byName.getByRole('button', { name: 'Go' }))
    const fnested = byName.frameLocator('iframe')
    void [ft, fc, fv, fd, fp, fkids, fchild, fnested]
  }
  void [pop, openedBy, mainFr, allFr, byUrl]
  page.on('response', (r) => void r.status()).on('requestfailed', (r) => void r.failure()?.errorText)
  page.on('dialog', (d) => d.accept(d.defaultValue()))
  const resp = await page.waitForResponse('**/api', { timeout: 1000 })
  const data: unknown = await resp.json()
  const req = await page.waitForRequest((r) => r.method() === 'POST')
  void [data, req.postData()]
  const context = await browser.newContext({ userAgent: 'Ctx', storageState: 'state.json', baseURL: 'https://example.com', bypassCSP: true, httpCredentials: { username: 'u', password: 'p' } })
  await context.addCookies([{ name: 'c', value: 'v', url: 'https://example.com' }])
  await context.clearCookies()
  await context.route('**/api', (route) => route.abort())
  const only = await context.newPage()
  await only.emulateMedia({ media: 'print', colorScheme: 'dark' })
  void [only.getByAltText('Logo', { exact: true }), only.getByTitle('Tooltip')]
  const filtered = only
    .locator('li', { has: only.locator('b'), hasNotText: /skip/i })
    .filter({ hasNot: only.locator('.bad'), hasText: 'ok' })
    .and(only.locator('.row'))
    .or(only.getByRole('button', { name: /^Save/, level: 2, pressed: true, checked: false, disabled: false, expanded: true, includeHidden: true, selected: false }))
  const _n: number = await filtered.count()
  const _items = await filtered.all()
  const _texts: string[] = await filtered.allTextContents()
  const _inners: string[] = await filtered.allInnerTexts()
  const _it: string = await only.innerText('#a', { strict: true, timeout: 100 })
  const _ih: string = await filtered.innerHTML({ timeout: 100 })
  const _iv: string = await filtered.inputValue()
  const _attr: string | null = await filtered.getAttribute('href')
  const _flags: boolean[] = [
    await only.isVisible('#a', { strict: false }),
    await filtered.isHidden(),
    await filtered.isEnabled(),
    await filtered.isDisabled(),
    await filtered.isChecked(),
    await filtered.isEditable(),
  ]
  void [_n, _items, _texts, _inners, _it, _ih, _iv, _attr, _flags]
  if (false) await chromium.connectOverCDP('ws://127.0.0.1:9222')
  const closed: boolean = only.isClosed()
  const currentUrl: string = only.url()
  await only.reload({ waitUntil: 'load', timeout: 5000 })
  await only.goBack({ waitUntil: 'domcontentloaded' })
  await only.goForward()
  await only.setContent('<p>ok</p>', { waitUntil: 'networkidle' })
  await only.waitForLoadState('networkidle', { timeout: 5000 })
  await only.waitForURL('**/done', { waitUntil: 'commit', timeout: 2000 })
  await only.waitForURL((u: URL) => u.pathname === '/done')
  const wf1: string = await only.waitForFunction(() => document.title)
  const wf2: number = await only.waitForFunction((n: number) => n + 1, 4, { polling: 50, timeout: 1000 })
  const wf3: unknown = await only.waitForFunction('1 + 1', undefined, { polling: 'raf' })
  await context.addInitScript('window.__ctxStr = 1')
  await context.addInitScript((n: number) => {
    ;(window as any).__ctxArg = n
  }, 7)
  await context.exposeFunction('ctxAdd', (a: number, b: number) => a + b)
  const cdp = await context.newCDPSession(only)
  cdp.on('Runtime.consoleAPICalled', (ev) => void ev)
  const cdpEval: unknown = await cdp.send('Runtime.evaluate', { expression: '1 + 1', returnByValue: true })
  await cdp.detach()
  if (byName) {
    const fcdp = await context.newCDPSession(byName)
    await fcdp.detach()
  }
  await only.addInitScript({ path: 'init.js' })
  await only.exposeFunction('pageEcho', async (x: string) => x)
  await only.addScriptTag({ content: 'window.__tag = 1', type: 'module' })
  await only.addStyleTag({ content: 'body { color: red; }' })
  const cookies: { name: string }[] = await context.cookies(['https://example.com'])
  await context.clearCookies({ name: 'c', domain: /example\.com/, path: '/' })
  const idbState = await context.storageState({ indexedDB: true })
  const apiRes = await context.request.get('/v1', { params: { a: '1' } })
  const apiBody: unknown = await apiRes.json()
  const pageReq = await only.request.post('/v1', { data: { ok: true }, multipart: { f: { name: 'a.txt', mimeType: 'text/plain', buffer: Buffer.from('hi') } } })
  await page.route('**/proxy/*', async (r) => {
    const upstream = await r.fetch()
    await r.fulfill({ response: upstream, json: { patched: true } })
  })
  await page.routeFromHAR('out.har', { url: '**/api/*', notFound: 'fallback' })
  await context.routeFromHAR('ctx.har', { update: true, updateMode: 'full', updateContent: 'embed' })
  const harPage = await browser.newPage({ recordHar: { path: 'p.har', mode: 'minimal', content: 'omit', urlFilter: /api/ } })
  await harPage.close()
  if (false) {
    const customBrowser = await chromium.launch({
      args: ['--disable-gpu'],
      ignoreDefaultArgs: ['--mute-audio'],
      channel: 'chrome',
      prewarmContexts: 2,
    })
    const customCtx = await customBrowser.newContext({
      resourcePreset: 'scraping',
      blockResources: ['image', 'stylesheet'],
      blockUrlPatterns: ['**/ads/**'],
    })
    const customPage = await customBrowser.newPage({ resourcePreset: 'minimal' })
    // @ts-expect-error not a valid resource preset
    await customBrowser.newPage({ resourcePreset: 'all' })
    // @ts-expect-error not a valid resource type
    await customBrowser.newPage({ blockResources: ['websocket'] })
    // @ts-expect-error prewarmContexts must be a number
    await chromium.launch({ prewarmContexts: 'two' })
    void [customCtx, customPage]
  }
  void [closed, currentUrl, wf1, wf2, wf3, cookies, idbState, apiBody, pageReq.headersArray(), cdpEval, context.pages().length, context.browser()]

  const pageAssert: PageAssertions = expect(page)
  const locAssert: LocatorAssertions = expect(item)
  const softExpect = expect.configure({ timeout: 2_000 })
  await pageAssert.toHaveTitle('Home')
  await pageAssert.not.toHaveTitle(/Err/, { timeout: 1_000 })
  await softExpect(page).toHaveURL(/example\.com/)
  await locAssert.toBeVisible({ timeout: 500 })
  await locAssert.not.toBeHidden()
  await locAssert.toBeAttached()
  await locAssert.toBeEnabled()
  await locAssert.toBeDisabled()
  await locAssert.toBeChecked({ checked: false })
  await locAssert.toBeEditable()
  await locAssert.toHaveText(/ok/i, { timeout: 500 })
  await locAssert.toContainText('ok')
  await locAssert.toHaveValue('abc')
  await locAssert.toHaveAttribute('data-id', /^12/)
  await locAssert.toHaveClass('active')
  await locAssert.toHaveCount(1)
  await fluxwright.expect(item).not.toHaveText('nope')
  // @ts-expect-error toBeVisible is only on LocatorAssertions, not PageAssertions
  await expect(page).toBeVisible()
  // @ts-expect-error toHaveURL is only on PageAssertions, not LocatorAssertions
  await expect(item).toHaveURL('https://example.com')

  const tracing: Tracing = context.tracing
  await tracing.start({ name: 'suite', title: 'smoke', screenshots: true, snapshots: true })
  await tracing.startChunk({ name: 'chunk-1', title: 'first chunk' })
  await tracing.stopChunk({ path: 'chunk.zip' })
  await tracing.stop({ path: 'trace.zip' })
  await tracing.stop()
  // @ts-expect-error tracing.start does not accept path
  await tracing.start({ path: 'bad.zip' })
  // @ts-expect-error tracing.stopChunk does not accept screenshots
  await tracing.stopChunk({ screenshots: true })

  const recOpts: RecordVideoOptions = { dir: 'videos', size: { width: 640, height: 480 } }
  const vidCtx = await browser.newContext({ recordVideo: recOpts })
  const vidPage = await vidCtx.newPage()
  const video: Video | null = vidPage.video()
  if (video) {
    const vp: string = await video.path()
    await video.saveAs('out.avi')
    await video.delete()
    void vp
  }
  const shotOpts: ToHaveScreenshotOptions = {
    threshold: 0.2,
    maxDiffPixels: 5,
    maxDiffPixelRatio: 0.01,
    animations: 'disabled',
    timeout: 2000,
    update: true,
    omitBackground: false,
  }
  await pageAssert.toHaveScreenshot('page.png', { ...shotOpts, fullPage: true, clip: { x: 0, y: 0, width: 10, height: 10 } })
  await locAssert.toHaveScreenshot(['shots', 'item.png'], shotOpts)
  // @ts-expect-error fullPage is only on PageAssertions.toHaveScreenshot
  await locAssert.toHaveScreenshot('item.png', { fullPage: true })
  // @ts-expect-error invalid animations mode
  await pageAssert.toHaveScreenshot('page.png', { animations: 'paused' })
  await vidCtx.close()

  await context.close()
  await browser.close()
}
