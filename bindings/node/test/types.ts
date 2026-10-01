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
  await fluxwright.chromium.launch()
  void [title, sum, upper, expression, wrong, png]

  const state: StorageState = await page.storageState({ path: 'state.json' })
  await browser.newPage({ storageState: state, locale: 'de-DE', colorScheme: 'dark', permissions: ['geolocation'] })
  await browser.newPage({ storageState: 'state.json', geolocation: { latitude: 1, longitude: 2 } })
  // @ts-expect-error not a color scheme
  await browser.newPage({ colorScheme: 'purple' })

  const item = page.getByRole('listitem').filter({ hasText: 'x' }).nth(1).getByTestId('price')
  const box: { x: number; width: number } | null = await item.boundingBox()
  const length: number = await item.evaluate((el: { textContent: string }, n: number) => el.textContent.length + n, 1)
  const shot: Buffer = await page.getByLabel('Email').screenshot({ path: 'el.png' })
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
  page.on('request', () => {})
  await browser.close()
}
