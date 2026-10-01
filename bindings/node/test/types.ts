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
  await browser.close()
}
