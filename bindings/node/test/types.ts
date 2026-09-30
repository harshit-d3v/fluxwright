// Compile-only check of the published typings: `npm run test:types`.
import fluxwright, { chromium } from '../addon'

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
  await browser.close()
}
