export * from './index'
import { Chromium, ConsoleMessage, NewPageOptions, Page, ScreenshotOptions, StorageState, Browser } from './index'
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
  /** Why a failed request failed (`requestfailed`); null otherwise. */
  failure(): { errorText: string } | null
}

/** A response, as `page.on('response')` and `page.waitForResponse` hand it over. */
export interface Response {
  url(): string
  status(): number
  statusText(): string
  /** Status 200 to 299. */
  ok(): boolean
  /** Names lower-cased. */
  headers(): Record<string, string>
  request(): Request
  /** The body, once the response finished loading. */
  body(): Promise<Buffer>
  text(): Promise<string>
  json(): Promise<any>
}

/**
 * A JavaScript dialog, as `page.on('dialog')` hands it over. Answer it with `accept` or
 * `dismiss`; one no listener answered is dismissed, so the page never hangs.
 */
export interface Dialog {
  type(): 'alert' | 'confirm' | 'prompt' | 'beforeunload'
  message(): string
  /** A prompt's default text. */
  defaultValue(): string
  /** `promptText` is what a `prompt` returns. */
  accept(promptText?: string): Promise<void>
  dismiss(): Promise<void>
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

/**
 * A browser context, as Playwright's: the options its page opens with. Each context holds one page,
 * so one job's cookies and storage never reach another: `newPage()` opens it and a second call
 * rejects. To share a login across pages, pass one page's `storageState()` to the next `newContext`.
 */
export interface BrowserContext {
  browser(): Browser
  /** The context's page while it is open. */
  pages(): Page[]
  newPage(): Promise<Page>
  /** `page.route` for this context's page; added before `newPage()`, it applies from the first request. */
  route(url: URLMatch, handler: (route: Route, request: Request) => unknown): Promise<void>
  unroute(url: URLMatch, handler?: (route: Route, request: Request) => unknown): Promise<void>
  /** The cookies and localStorage of this context's page, as `page.storageState`. */
  storageState(options?: { path?: string }): Promise<StorageState>
  /** Every cookie in this context. */
  cookies(): Promise<StorageState['cookies']>
  /** Closes the context's page; the context opens no other. */
  close(): Promise<void>
}

declare module './index' {
  interface Browser {
    /** A browser context with `newPage`'s options, as Playwright's `newContext`. */
    newContext(options?: NewPageOptions): Promise<BrowserContext>
  }
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
    on(event: 'request' | 'requestfinished' | 'requestfailed', handler: (request: Request) => void): this
    on(event: 'response', handler: (response: Response) => void): this
    on(event: 'dialog', handler: (dialog: Dialog) => unknown): this
    once(event: 'console', handler: (message: ConsoleMessage) => void): this
    once(event: 'pageerror', handler: (error: Error) => void): this
    once(event: 'request' | 'requestfinished' | 'requestfailed', handler: (request: Request) => void): this
    once(event: 'response', handler: (response: Response) => void): this
    once(event: 'dialog', handler: (dialog: Dialog) => unknown): this
    off(
      event: 'console' | 'pageerror' | 'request' | 'response' | 'requestfinished' | 'requestfailed' | 'dialog',
      handler: (...args: any[]) => unknown,
    ): this
    /**
     * The first response from now on whose URL matches (a glob or RegExp, as in `page.route`) or
     * that the predicate accepts. Default timeout 30 s; 0 waits forever.
     */
    waitForResponse(
      urlOrPredicate: string | RegExp | ((response: Response) => boolean | Promise<boolean>),
      options?: { timeout?: number },
    ): Promise<Response>
    /** `waitForResponse`, for requests. */
    waitForRequest(
      urlOrPredicate: string | RegExp | ((request: Request) => boolean | Promise<boolean>),
      options?: { timeout?: number },
    ): Promise<Request>
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
