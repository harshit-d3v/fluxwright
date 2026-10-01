export * from './index'
import { Chromium, ScreenshotOptions, StorageState } from './index'
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
    /** Also writes the state to `path` as JSON, which `newPage({ storageState: path })` reads. */
    storageState(options?: { path?: string }): Promise<StorageState>
    /** Exceptions nothing caught so far, as `Error` objects. */
    pageErrors(): Promise<Error[]>
    /** Also writes the PNG to `path`. */
    screenshot(options?: ScreenshotOptions & { path?: string }): Promise<Buffer>
  }
  interface Locator {
    /**
     * Calls `pageFunction` with the element and `arg` in the page, as Playwright does, and returns
     * its result. The function is sent as source text: it can't use variables from Node.
     */
    evaluate<R, Arg = undefined>(pageFunction: (element: any, arg: Arg) => R, arg?: Arg): Promise<Awaited<R>>
    /** Also writes the PNG to `path`. */
    screenshot(options?: { path?: string }): Promise<Buffer>
  }
}
