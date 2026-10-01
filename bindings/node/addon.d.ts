export * from './index'
import { Chromium, ConsoleMessage, ScreenshotOptions, StorageState } from './index'
/** Playwright-style export: `chromium.launch(...)`. Same as `Chromium.launch`. */
export const chromium: typeof Chromium
declare const fluxwright: { chromium: typeof Chromium }
export default fluxwright

/** A request as a `page.route` handler sees it, as in Playwright. */
export interface Request {
  url(): string
  method(): string
  headers(): Record<string, string>
  postData(): string | null
  /** `document`, `script`, `stylesheet`, `image`, `xhr`, ... */
  resourceType(): string
}

/** How a `page.route` handler answers, as in Playwright. Each request takes one answer. */
export interface Route {
  request(): Request
  /** A made-up response. `json` is serialized and sets the content type; `path` reads a file. */
  fulfill(options?: {
    status?: number
    headers?: Record<string, string>
    contentType?: string
    body?: string | Buffer
    json?: unknown
    path?: string
  }): Promise<void>
  /** Sends the request on, changed if asked; `headers` replaces all of them. */
  continue(options?: { url?: string; method?: string; headers?: Record<string, string>; postData?: string | Buffer }): Promise<void>
  /** Fails it with one of Playwright's codes: `failed` (default), `aborted`, `blockedbyclient`, `timedout`, ... */
  abort(errorCode?: string): Promise<void>
  /** Leaves it to the next matching handler, the one added before this one. */
  fallback(): Promise<void>
}

/** A glob (`**` any characters, `*` any but `/`, `{a,b}` either), a RegExp, or a function of the URL. */
export type URLMatch = string | RegExp | ((url: URL) => boolean)

export interface Download {
  url(): string
  /** From `Content-Disposition` or the URL. */
  suggestedFilename(): string
  /** The temporary file, deleted when the page closes. */
  path(): Promise<string>
  /** Copies the file to `path`, creating missing folders. */
  saveAs(path: string): Promise<void>
  failure(): Promise<string | null>
}

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
    /**
     * Hands matching requests of this page, its popups and iframes to `handler`, as Playwright's
     * `page.route`. The handler added last runs first. Call it before navigating.
     */
    route(url: URLMatch, handler: (route: Route, request: Request) => unknown): Promise<void>
    /** Removes the handlers added for `url` (only `handler`, when given). */
    unroute(url: URLMatch, handler?: (route: Route, request: Request) => unknown): Promise<void>
    on(event: 'console', handler: (message: ConsoleMessage) => void): this
    on(event: 'pageerror', handler: (error: Error) => void): this
    once(event: 'console', handler: (message: ConsoleMessage) => void): this
    once(event: 'pageerror', handler: (error: Error) => void): this
    off(event: 'console' | 'pageerror', handler: (...args: any[]) => void): this
    /** The next download this page finished; ones that finished earlier queue up. Default timeout 30 s. */
    waitForDownload(options?: { timeout?: number }): Promise<Download>
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
