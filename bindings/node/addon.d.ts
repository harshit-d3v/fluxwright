export * from './index'
import { Chromium } from './index'
/** Playwright-style export: `chromium.launch(...)`. Same as `Chromium.launch`. */
export const chromium: typeof Chromium
declare const fluxwright: { chromium: typeof Chromium }
export default fluxwright

declare module './index' {
  interface Page {
    /**
     * Runs `pageFunction` in the page with `arg` and returns its result, awaiting promises.
     * As in Playwright, the function is sent as source text: it can use `arg` (JSON-serializable)
     * but not variables from Node.
     */
    evaluate<R, Arg = undefined>(pageFunction: (arg: Arg) => R, arg?: Arg): Promise<Awaited<R>>
  }
}
