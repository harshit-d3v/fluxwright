// Compile-only check of the published typings: `npm run test:types`.
import fluxwright, { chromium, type StorageState } from '../addon'

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
  // @ts-expect-error webp is not a screenshot type
  await page.screenshot({ type: 'webp' })
  await fluxwright.chromium.launch()
  void [title, sum, upper, expression, wrong, png, jpeg, pdf]

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
  page.on('popup', () => {})
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
  await only.addInitScript({ path: 'init.js' })
  await only.exposeFunction('pageEcho', async (x: string) => x)
  await only.addScriptTag({ content: 'window.__tag = 1', type: 'module' })
  await only.addStyleTag({ content: 'body { color: red; }' })
  const cookies: { name: string }[] = await context.cookies()
  void [closed, currentUrl, wf1, wf2, wf3, cookies, context.pages().length, context.browser()]
  await context.close()
  await browser.close()
}
