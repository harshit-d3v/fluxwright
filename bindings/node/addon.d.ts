export * from './index'
import {
  Browser,
  Chromium,
  ConsoleMessage,
  FrameLocator,
  GetByRoleOptions,
  GetByTextOptions,
  GotoOptions,
  Locator,
  NewPageOptions,
  Page,
  PdfOptions,
  ScreenshotOptions,
  StorageState,
} from './index'

export interface FilterOptions {
  has?: Locator
  hasNot?: Locator
  hasText?: string | RegExp
  hasNotText?: string | RegExp
}
export interface ExpectOptions {
  timeout?: number
}

export interface LocatorAssertions {
  readonly not: LocatorAssertions
  toBeVisible(options?: ExpectOptions): Promise<void>
  toBeHidden(options?: ExpectOptions): Promise<void>
  toBeAttached(options?: ExpectOptions): Promise<void>
  toBeEnabled(options?: ExpectOptions): Promise<void>
  toBeDisabled(options?: ExpectOptions): Promise<void>
  toBeChecked(options?: ExpectOptions & { checked?: boolean }): Promise<void>
  toBeEditable(options?: ExpectOptions): Promise<void>
  toHaveText(expected: string | RegExp, options?: ExpectOptions): Promise<void>
  toContainText(expected: string | RegExp, options?: ExpectOptions): Promise<void>
  toHaveValue(value: string | RegExp, options?: ExpectOptions): Promise<void>
  toHaveAttribute(name: string, value: string | RegExp, options?: ExpectOptions): Promise<void>
  toHaveClass(expected: string | RegExp, options?: ExpectOptions): Promise<void>
  toHaveCount(count: number, options?: ExpectOptions): Promise<void>
}

export interface PageAssertions {
  readonly not: PageAssertions
  toHaveTitle(titleOrRegExp: string | RegExp, options?: ExpectOptions): Promise<void>
  toHaveURL(urlOrRegExp: string | RegExp, options?: ExpectOptions): Promise<void>
}

export interface Expect {
  (page: Page): PageAssertions
  (locator: Locator): LocatorAssertions
  configure(options: ExpectOptions): Expect
}

export const expect: Expect

/**
 * Playwright-style export: `chromium.launch(...)`, which is `Chromium.launch` after a check that
 * every option is one Fluxwright implements (others throw a TypeError).
 */
export const chromium: Pick<typeof Chromium, 'launch'> & {
  connectOverCDP(endpointURL: string, options?: Record<string, never>): Promise<Browser>
}
declare const fluxwright: { chromium: typeof chromium; expect: typeof expect }
export default fluxwright

/** Playwright's `page.pdf` options. */
export type PDFOptions = Omit<PdfOptions, 'width' | 'height' | 'margin'> & {
  width?: string | number
  height?: string | number
  margin?: { top?: string | number; right?: string | number; bottom?: string | number; left?: string | number }
  path?: string
}

/**
 * A request, as in Playwright: the same object for all of one request's events and for
 * `response.request()`.
 */
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

export interface IndexedDBDatabase {
  name: string
  version: number
  stores: Array<{
    name: string
    autoIncrement: boolean
    keyPath?: string
    keyPathArray?: Array<string>
    records: Array<{ key?: any; keyEncoded?: any; value?: any; valueEncoded?: any }>
    indexes: Array<{ name: string; keyPath?: string; keyPathArray?: Array<string>; multiEntry: boolean; unique: boolean }>
  }>
}

export interface APIResponse {
  url(): string
  status(): number
  statusText(): string
  ok(): boolean
  headers(): Record<string, string>
  headersArray(): Array<{ name: string; value: string }>
  body(): Promise<Buffer>
  text(): Promise<string>
  json(): Promise<any>
  dispose(): Promise<void>
}

export interface APIRequestOptions {
  method?: string
  headers?: Record<string, string>
  params?: Record<string, string | number | boolean> | URLSearchParams | string
  data?: string | Buffer | unknown
  form?: Record<string, string | number | boolean> | URLSearchParams
  multipart?: FormData | Record<string, string | number | boolean | NodeJS.ReadableStream | { name: string; mimeType: string; buffer: Buffer }>
  failOnStatusCode?: boolean
  maxRedirects?: number
  timeout?: number
}

export interface APIRequestContext {
  fetch(url: string, options?: APIRequestOptions): Promise<APIResponse>
  get(url: string, options?: Omit<APIRequestOptions, 'method'>): Promise<APIResponse>
  post(url: string, options?: Omit<APIRequestOptions, 'method'>): Promise<APIResponse>
  put(url: string, options?: Omit<APIRequestOptions, 'method'>): Promise<APIResponse>
  patch(url: string, options?: Omit<APIRequestOptions, 'method'>): Promise<APIResponse>
  delete(url: string, options?: Omit<APIRequestOptions, 'method'>): Promise<APIResponse>
  head(url: string, options?: Omit<APIRequestOptions, 'method'>): Promise<APIResponse>
  storageState(options?: { path?: string }): Promise<StorageState>
  dispose(): Promise<void>
}

/** How a `page.route` handler answers, as in Playwright. Each request takes one answer. */
export interface Route {
  request(): Request
  /** Performs the request without fulfilling it, so the response can be inspected or modified. */
  fetch(options?: {
    url?: string
    method?: string
    headers?: Record<string, string>
    postData?: string | Buffer | unknown
    maxRedirects?: number
    timeout?: number
  }): Promise<APIResponse>
  /** A made-up response. `json` is serialized and sets the content type; `path` reads a file. */
  fulfill(options?: {
    status?: number
    headers?: Record<string, string>
    contentType?: string
    body?: string | Buffer
    json?: unknown
    path?: string
    response?: APIResponse
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

export interface WebSocket {
  url(): string
  isClosed(): boolean
  on(event: 'framesent' | 'framereceived', handler: (data: { payload: string | Buffer }) => unknown): this
  on(event: 'socketerror', handler: (error: string) => unknown): this
  on(event: 'close', handler: (webSocket: WebSocket) => unknown): this
  once(event: 'framesent' | 'framereceived', handler: (data: { payload: string | Buffer }) => unknown): this
  once(event: 'socketerror', handler: (error: string) => unknown): this
  once(event: 'close', handler: (webSocket: WebSocket) => unknown): this
  addListener(event: 'framesent' | 'framereceived', handler: (data: { payload: string | Buffer }) => unknown): this
  addListener(event: 'socketerror', handler: (error: string) => unknown): this
  addListener(event: 'close', handler: (webSocket: WebSocket) => unknown): this
  off(event: string, handler: (...args: any[]) => unknown): this
  removeListener(event: string, handler: (...args: any[]) => unknown): this
  waitForEvent(
    event: 'framesent' | 'framereceived',
    optionsOrPredicate?:
      | { predicate?: (data: { payload: string | Buffer }) => boolean | Promise<boolean>; timeout?: number }
      | ((data: { payload: string | Buffer }) => boolean | Promise<boolean>),
  ): Promise<{ payload: string | Buffer }>
  waitForEvent(
    event: 'socketerror',
    optionsOrPredicate?: { predicate?: (error: string) => boolean | Promise<boolean>; timeout?: number } | ((error: string) => boolean | Promise<boolean>),
  ): Promise<string>
  waitForEvent(
    event: 'close',
    optionsOrPredicate?: { predicate?: (webSocket: WebSocket) => boolean | Promise<boolean>; timeout?: number } | ((webSocket: WebSocket) => boolean | Promise<boolean>),
  ): Promise<WebSocket>
}

export interface Worker {
  url(): string
  evaluate<R, Arg = undefined>(pageFunction: (arg: Arg) => R, arg?: Arg): Promise<Awaited<R>>
  evaluate(pageFunction: string, arg?: unknown): Promise<any>
  on(event: 'close', handler: (worker: Worker) => unknown): this
  on(event: 'console', handler: (message: ConsoleMessage) => unknown): this
  once(event: 'close', handler: (worker: Worker) => unknown): this
  once(event: 'console', handler: (message: ConsoleMessage) => unknown): this
  addListener(event: 'close', handler: (worker: Worker) => unknown): this
  addListener(event: 'console', handler: (message: ConsoleMessage) => unknown): this
  off(event: 'close' | 'console', handler: (...args: any[]) => unknown): this
  removeListener(event: 'close' | 'console', handler: (...args: any[]) => unknown): this
  waitForEvent(
    event: 'close',
    optionsOrPredicate?: { predicate?: (worker: Worker) => boolean | Promise<boolean>; timeout?: number } | ((worker: Worker) => boolean | Promise<boolean>),
  ): Promise<Worker>
  waitForEvent(
    event: 'console',
    optionsOrPredicate?: { predicate?: (message: ConsoleMessage) => boolean | Promise<boolean>; timeout?: number } | ((message: ConsoleMessage) => boolean | Promise<boolean>),
  ): Promise<ConsoleMessage>
}

export interface RouteFromHarOptions {
  url?: string | RegExp
  notFound?: 'abort' | 'fallback'
  update?: boolean
  updateContent?: 'embed' | 'omit'
  updateMode?: 'full' | 'minimal'
}

export type KeyModifier = 'Alt' | 'Control' | 'Meta' | 'Shift' | 'ControlOrMeta'
export type MouseButton = 'left' | 'right' | 'middle'

export interface FilePayload {
  name: string
  mimeType: string
  buffer: Buffer
}

export type InputFiles = string | Array<string> | FilePayload | Array<FilePayload>

export interface SelectOptionFilter {
  value?: string
  label?: string
  index?: number
}

export interface ClickOptions {
  button?: MouseButton
  clickCount?: number
  delay?: number
  position?: { x: number; y: number }
  modifiers?: Array<KeyModifier>
  force?: boolean
  trial?: boolean
  timeout?: number
}

export interface DblclickOptions {
  button?: MouseButton
  delay?: number
  position?: { x: number; y: number }
  modifiers?: Array<KeyModifier>
  force?: boolean
  trial?: boolean
  timeout?: number
}

export interface HoverOptions {
  position?: { x: number; y: number }
  modifiers?: Array<KeyModifier>
  force?: boolean
  trial?: boolean
  timeout?: number
}

export interface CheckOptions {
  position?: { x: number; y: number }
  force?: boolean
  trial?: boolean
  timeout?: number
}

export interface FileChooser {
  page(): Page
  isMultiple(): boolean
  setFiles(files: InputFiles, options?: { timeout?: number }): Promise<void>
}

export interface Mouse {
  move(x: number, y: number, options?: { steps?: number }): Promise<void>
  down(options?: { button?: MouseButton; clickCount?: number }): Promise<void>
  up(options?: { button?: MouseButton; clickCount?: number }): Promise<void>
  click(x: number, y: number, options?: { button?: MouseButton; clickCount?: number; delay?: number }): Promise<void>
  dblclick(x: number, y: number, options?: { button?: MouseButton; delay?: number }): Promise<void>
  wheel(deltaX: number, deltaY: number): Promise<void>
}

export interface Keyboard {
  down(key: string): Promise<void>
  up(key: string): Promise<void>
  press(key: string, options?: { delay?: number }): Promise<void>
  type(text: string, options?: { delay?: number }): Promise<void>
  insertText(text: string): Promise<void>
}

export interface Touchscreen {
  tap(x: number, y: number): Promise<void>
}

/**
 * A browser context, as Playwright's: holds multiple pages and popups sharing cookies, storage,
 * init scripts, exposed functions and routes.
 */
export interface BrowserContext {
  browser(): Browser
  /** The context's open pages, including popups. */
  pages(): Page[]
  newPage(): Promise<Page>
  /** `page.route` for all pages and popups in this context; added before `newPage()`, it applies from the first request. */
  route(url: URLMatch, handler: (route: Route, request: Request) => unknown): Promise<void>
  unroute(url: URLMatch, handler?: (route: Route, request: Request) => unknown): Promise<void>
  routeFromHAR(har: string, options?: RouteFromHarOptions): Promise<void>
  addInitScript<Arg>(script: (arg: Arg) => unknown, arg: Arg): Promise<void>
  addInitScript(script: (() => unknown) | string | { path?: string; content?: string }): Promise<void>
  exposeFunction(name: string, callback: (...args: any[]) => unknown): Promise<void>
  /** The cookies, localStorage, and optionally IndexedDB of this context, usable before or after `newPage()`. */
  storageState(options?: { path?: string; indexedDB?: boolean }): Promise<StorageState>
  /** Cookies in this context (all or matching `urls`). */
  cookies(urls?: string | Array<string>): Promise<StorageState['cookies']>
  addCookies(
    cookies: Array<{
      name: string
      value: string
      url?: string
      domain?: string
      path?: string
      expires?: number
      httpOnly?: boolean
      secure?: boolean
      sameSite?: 'Strict' | 'Lax' | 'None'
      partitionKey?: string
      _crHasCrossSiteAncestor?: boolean
    }>,
  ): Promise<void>
  clearCookies(options?: { name?: string | RegExp; domain?: string | RegExp; path?: string | RegExp }): Promise<void>
  readonly request: APIRequestContext
  readonly tracing: Tracing
  newCDPSession(page: Page | Frame): Promise<CDPSession>
  /** Closes this context and all of its pages and popups. */
  close(): Promise<void>
}

export interface TracingStartOptions {
  /** Trace archive logical context name. */
  name?: string
  /** Display title shown in Playwright Trace Viewer. */
  title?: string
  /** Whether to capture screenshots / screencast frames during tracing. */
  screenshots?: boolean
  /** Whether to record DOM snapshot metadata. */
  snapshots?: boolean
}

export interface TracingChunkOptions {
  /** Logical chunk identifier. */
  name?: string
  /** Display title shown for this trace chunk. */
  title?: string
}

export interface TracingStopOptions {
  /** Destination path to write the Playwright trace `.zip` bundle. */
  path?: string
}

export interface Tracing {
  start(options?: TracingStartOptions): Promise<void>
  startChunk(options?: TracingChunkOptions): Promise<void>
  stopChunk(options?: TracingStopOptions): Promise<void>
  stop(options?: TracingStopOptions): Promise<void>
}

export interface CDPSession {
  send(method: string, params?: Record<string, any>): Promise<any>
  on(event: string, handler: (params: any) => void): this
  once(event: string, handler: (params: any) => void): this
  addListener(event: string, handler: (params: any) => void): this
  off(event: string, handler: (params: any) => void): this
  removeListener(event: string, handler: (params: any) => void): this
  detach(): Promise<void>
}

/** An imperative handle to a main frame or iframe on a page. */
export interface Frame {
  page(): Page
  url(): string
  name(): string
  isDetached(): boolean
  parentFrame(): Frame | null
  childFrames(): Frame[]
  goto(url: string, options?: GotoOptions): Promise<void>
  evaluate<R, Arg = undefined>(pageFunction: (arg: Arg) => R, arg?: Arg): Promise<Awaited<R>>
  evaluate(pageFunction: string, arg?: unknown): Promise<any>
  title(): Promise<string>
  content(): Promise<string>
  locator(selector: string, options?: FilterOptions): Locator
  getByText(text: string, options?: GetByTextOptions): Locator
  getByRole(role: string, options?: GetByRoleOptions): Locator
  getByLabel(text: string, options?: GetByTextOptions): Locator
  getByPlaceholder(text: string, options?: GetByTextOptions): Locator
  getByAltText(text: string, options?: GetByTextOptions): Locator
  getByTitle(text: string, options?: GetByTextOptions): Locator
  getByTestId(testId: string): Locator
  frameLocator(selector: string): FrameLocator
}

declare module './index' {
  interface Browser {
    /** A browser context with `newPage`'s options, as Playwright's `newContext`. */
    newContext(options?: NewPageOptions): Promise<BrowserContext>
  }
  interface FrameLocator {
    locator(selector: string, options?: FilterOptions): Locator
  }
  interface Page {
    locator(selector: string, options?: FilterOptions): Locator
    /**
     * Runs `pageFunction` in the page with `arg` and returns its result, awaiting promises.
     * As in Playwright, the function is sent as source text: it can use `arg` (JSON-serializable)
     * but not variables from Node.
     */
    evaluate<R, Arg = undefined>(pageFunction: (arg: Arg) => R, arg?: Arg): Promise<Awaited<R>>
    /** Also writes the state to `path` as JSON, which `newPage({ storageState: path })` reads. */
    storageState(options?: { path?: string; indexedDB?: boolean }): Promise<StorageState>
    readonly request: APIRequestContext
    /** Exceptions nothing caught so far, as `Error` objects. */
    pageErrors(): Promise<Error[]>
    /** Also writes the image to `path`; without `type`, its extension picks PNG or JPEG. */
    screenshot(options?: ScreenshotOptions & { path?: string }): Promise<Buffer>
    /** Changes CSS `media` and/or `prefers-color-scheme`; `null` resets either. */
    emulateMedia(options?: {
      media?: 'screen' | 'print' | null
      colorScheme?: 'light' | 'dark' | 'no-preference' | null
    }): Promise<void>
    /**
     * The page printed to PDF, as Playwright's `page.pdf` (headless only). Lengths are numbers of
     * pixels or strings with `px`, `in`, `cm` or `mm`. Also writes the file to `path`.
     */
    pdf(options?: PDFOptions): Promise<Buffer>
    /**
     * Hands matching requests of this page, its popups and iframes to `handler`, as Playwright's
     * `page.route`. The handler added last runs first. Call it before navigating. Answer before
     * the handler returns (await or return the answer): an unanswered request continues, and a
     * later answer is ignored with a warning.
     */
    route(url: URLMatch, handler: (route: Route, request: Request) => unknown): Promise<void>
    /** Removes the handlers added for `url` (only `handler`, when given). */
    unroute(url: URLMatch, handler?: (route: Route, request: Request) => unknown): Promise<void>
    routeFromHAR(har: string, options?: RouteFromHarOptions): Promise<void>
    workers(): Worker[]
    on(event: 'console', handler: (message: ConsoleMessage) => void): this
    on(event: 'pageerror', handler: (error: Error) => void): this
    on(event: 'request' | 'requestfinished' | 'requestfailed', handler: (request: Request) => void): this
    on(event: 'response', handler: (response: Response) => void): this
    on(event: 'dialog', handler: (dialog: Dialog) => unknown): this
    on(event: 'filechooser', handler: (fileChooser: FileChooser) => unknown): this
    on(event: 'popup', handler: (popup: Page) => unknown): this
    on(event: 'websocket', handler: (webSocket: WebSocket) => unknown): this
    on(event: 'worker', handler: (worker: Worker) => unknown): this
    once(event: 'console', handler: (message: ConsoleMessage) => void): this
    once(event: 'pageerror', handler: (error: Error) => void): this
    once(event: 'request' | 'requestfinished' | 'requestfailed', handler: (request: Request) => void): this
    once(event: 'response', handler: (response: Response) => void): this
    once(event: 'dialog', handler: (dialog: Dialog) => unknown): this
    once(event: 'filechooser', handler: (fileChooser: FileChooser) => unknown): this
    once(event: 'popup', handler: (popup: Page) => unknown): this
    once(event: 'websocket', handler: (webSocket: WebSocket) => unknown): this
    once(event: 'worker', handler: (worker: Worker) => unknown): this
    off(
      event:
        | 'console'
        | 'pageerror'
        | 'request'
        | 'response'
        | 'requestfinished'
        | 'requestfailed'
        | 'dialog'
        | 'filechooser'
        | 'popup'
        | 'websocket'
        | 'worker',
      handler: (...args: any[]) => unknown,
    ): this
    waitForEvent(
      event: 'console',
      optionsOrPredicate?: { predicate?: (message: ConsoleMessage) => boolean | Promise<boolean>; timeout?: number } | ((message: ConsoleMessage) => boolean | Promise<boolean>),
    ): Promise<ConsoleMessage>
    waitForEvent(
      event: 'pageerror',
      optionsOrPredicate?: { predicate?: (error: Error) => boolean | Promise<boolean>; timeout?: number } | ((error: Error) => boolean | Promise<boolean>),
    ): Promise<Error>
    waitForEvent(
      event: 'request' | 'requestfinished' | 'requestfailed',
      optionsOrPredicate?: { predicate?: (request: Request) => boolean | Promise<boolean>; timeout?: number } | ((request: Request) => boolean | Promise<boolean>),
    ): Promise<Request>
    waitForEvent(
      event: 'response',
      optionsOrPredicate?: { predicate?: (response: Response) => boolean | Promise<boolean>; timeout?: number } | ((response: Response) => boolean | Promise<boolean>),
    ): Promise<Response>
    waitForEvent(
      event: 'dialog',
      optionsOrPredicate?: { predicate?: (dialog: Dialog) => boolean | Promise<boolean>; timeout?: number } | ((dialog: Dialog) => boolean | Promise<boolean>),
    ): Promise<Dialog>
    waitForEvent(
      event: 'filechooser',
      optionsOrPredicate?: { predicate?: (fileChooser: FileChooser) => boolean | Promise<boolean>; timeout?: number } | ((fileChooser: FileChooser) => boolean | Promise<boolean>),
    ): Promise<FileChooser>
    waitForEvent(
      event: 'popup',
      optionsOrPredicate?: { predicate?: (popup: Page) => boolean | Promise<boolean>; timeout?: number } | ((popup: Page) => boolean | Promise<boolean>),
    ): Promise<Page>
    waitForEvent(
      event: 'websocket',
      optionsOrPredicate?: { predicate?: (webSocket: WebSocket) => boolean | Promise<boolean>; timeout?: number } | ((webSocket: WebSocket) => boolean | Promise<boolean>),
    ): Promise<WebSocket>
    waitForEvent(
      event: 'worker',
      optionsOrPredicate?: { predicate?: (worker: Worker) => boolean | Promise<boolean>; timeout?: number } | ((worker: Worker) => boolean | Promise<boolean>),
    ): Promise<Worker>
    /**
     * The first response from now on whose URL matches (a glob or RegExp, as in `page.route`) or
     * that the predicate accepts. Default timeout 30 s; 0 waits until the page closes.
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
    waitForFileChooser(options?: {
      predicate?: (fileChooser: FileChooser) => boolean | Promise<boolean>
      timeout?: number
    }): Promise<FileChooser>
    waitForPopup(options?: {
      predicate?: (popup: Page) => boolean | Promise<boolean>
      timeout?: number
    }): Promise<Page>
    opener(): Promise<Page | null>
    frames(): Frame[]
    mainFrame(): Frame
    frame(frameSelector: string | { name?: string; url?: URLMatch }): Frame | null
    /** The next download this page finished; ones that finished earlier queue up. Default timeout 30 s. */
    waitForDownload(options?: { timeout?: number }): Promise<Download>
    readonly mouse: Mouse
    readonly keyboard: Keyboard
    readonly touchscreen: Touchscreen
    /** `timeout` is in milliseconds, for this call only; 0 waits as long as it takes. */
    click(selector: string, options?: ClickOptions & { strict?: boolean }): Promise<void>
    dblclick(selector: string, options?: DblclickOptions & { strict?: boolean }): Promise<void>
    hover(selector: string, options?: HoverOptions & { strict?: boolean }): Promise<void>
    focus(selector: string, options?: { strict?: boolean; timeout?: number }): Promise<void>
    dragAndDrop(source: string, target: string, options?: { force?: boolean; strict?: boolean; timeout?: number }): Promise<void>
    fill(selector: string, value: string, options?: { strict?: boolean; timeout?: number }): Promise<void>
    press(selector: string, key: string, options?: { delay?: number; strict?: boolean; timeout?: number }): Promise<void>
    type(selector: string, text: string, options?: { delay?: number; strict?: boolean; timeout?: number }): Promise<void>
    check(selector: string, options?: CheckOptions & { strict?: boolean }): Promise<void>
    uncheck(selector: string, options?: CheckOptions & { strict?: boolean }): Promise<void>
    setChecked(selector: string, checked: boolean, options?: CheckOptions & { strict?: boolean }): Promise<void>
    selectOption(
      selector: string,
      values: string | SelectOptionFilter | Array<string | SelectOptionFilter> | null,
      options?: { force?: boolean; strict?: boolean; timeout?: number },
    ): Promise<Array<string>>
    setInputFiles(selector: string, files: InputFiles, options?: { strict?: boolean; timeout?: number }): Promise<void>
    tap(selector: string, options?: HoverOptions & { strict?: boolean }): Promise<void>
    waitForSelector(
      selector: string,
      options?: { state?: 'attached' | 'detached' | 'visible' | 'hidden'; strict?: boolean; timeout?: number },
    ): Promise<void>
    textContent(selector: string, options?: { strict?: boolean; timeout?: number }): Promise<string | null>
    innerText(selector: string, options?: { strict?: boolean; timeout?: number }): Promise<string>
    innerHTML(selector: string, options?: { strict?: boolean; timeout?: number }): Promise<string>
    inputValue(selector: string, options?: { strict?: boolean; timeout?: number }): Promise<string>
    getAttribute(selector: string, name: string, options?: { strict?: boolean; timeout?: number }): Promise<string | null>
    isVisible(selector: string, options?: { strict?: boolean; timeout?: number }): Promise<boolean>
    isHidden(selector: string, options?: { strict?: boolean; timeout?: number }): Promise<boolean>
    isEnabled(selector: string, options?: { strict?: boolean; timeout?: number }): Promise<boolean>
    isDisabled(selector: string, options?: { strict?: boolean; timeout?: number }): Promise<boolean>
    isChecked(selector: string, options?: { strict?: boolean; timeout?: number }): Promise<boolean>
    isEditable(selector: string, options?: { strict?: boolean; timeout?: number }): Promise<boolean>
    waitForLoadState(state?: 'load' | 'domcontentloaded' | 'networkidle', options?: { timeout?: number }): Promise<void>
    waitForURL(
      url: URLMatch,
      options?: { waitUntil?: 'load' | 'domcontentloaded' | 'networkidle' | 'commit'; timeout?: number },
    ): Promise<void>
    waitForFunction<R, Arg = undefined>(
      pageFunction: (arg: Arg) => R,
      arg?: Arg,
      options?: { polling?: 'raf' | number; timeout?: number },
    ): Promise<Awaited<R>>
    waitForFunction(
      pageFunction: string,
      arg?: unknown,
      options?: { polling?: 'raf' | number; timeout?: number },
    ): Promise<any>
    addInitScript<Arg>(script: (arg: Arg) => unknown, arg: Arg): Promise<void>
    addInitScript(script: (() => unknown) | string | { path?: string; content?: string }): Promise<void>
    exposeFunction(name: string, callback: (...args: any[]) => unknown): Promise<void>
    addScriptTag(options: { url?: string; path?: string; content?: string; type?: string }): Promise<void>
    addStyleTag(options: { url?: string; path?: string; content?: string }): Promise<void>
  }
  interface Locator {
    locator(selector: string, options?: FilterOptions): Locator
    filter(options: FilterOptions): Locator
    and(locator: Locator): Locator
    or(locator: Locator): Locator
    all(): Promise<Array<Locator>>
    /** `timeout` is in milliseconds, for this call only; 0 waits as long as it takes. */
    click(options?: ClickOptions): Promise<void>
    dblclick(options?: DblclickOptions): Promise<void>
    hover(options?: HoverOptions): Promise<void>
    focus(options?: { timeout?: number }): Promise<void>
    blur(options?: { timeout?: number }): Promise<void>
    scrollIntoViewIfNeeded(options?: { timeout?: number }): Promise<void>
    dragTo(target: Locator, options?: { force?: boolean; timeout?: number }): Promise<void>
    fill(value: string, options?: { timeout?: number }): Promise<void>
    press(key: string, options?: { delay?: number; timeout?: number }): Promise<void>
    pressSequentially(text: string, options?: { delay?: number; timeout?: number }): Promise<void>
    type(text: string, options?: { delay?: number; timeout?: number }): Promise<void>
    clear(options?: { force?: boolean; timeout?: number }): Promise<void>
    check(options?: CheckOptions): Promise<void>
    uncheck(options?: CheckOptions): Promise<void>
    setChecked(checked: boolean, options?: CheckOptions): Promise<void>
    selectOption(
      values: string | SelectOptionFilter | Array<string | SelectOptionFilter> | null,
      options?: { force?: boolean; timeout?: number },
    ): Promise<Array<string>>
    setInputFiles(files: InputFiles, options?: { timeout?: number }): Promise<void>
    tap(options?: HoverOptions): Promise<void>
    waitFor(options?: { state?: 'attached' | 'detached' | 'visible' | 'hidden'; timeout?: number }): Promise<void>
    textContent(options?: { timeout?: number }): Promise<string | null>
    innerText(options?: { timeout?: number }): Promise<string>
    innerHTML(options?: { timeout?: number }): Promise<string>
    inputValue(options?: { timeout?: number }): Promise<string>
    getAttribute(name: string, options?: { timeout?: number }): Promise<string | null>
    isVisible(options?: { timeout?: number }): Promise<boolean>
    isHidden(options?: { timeout?: number }): Promise<boolean>
    isEnabled(options?: { timeout?: number }): Promise<boolean>
    isDisabled(options?: { timeout?: number }): Promise<boolean>
    isChecked(options?: { timeout?: number }): Promise<boolean>
    isEditable(options?: { timeout?: number }): Promise<boolean>
    boundingBox(options?: { timeout?: number }): Promise<{ x: number; y: number; width: number; height: number } | null>
    /**
     * Calls `pageFunction` with the element and `arg` in the page, as Playwright does, and returns
     * its result. The function is sent as source text: it can't use variables from Node.
     */
    evaluate<R, Arg = undefined>(
      pageFunction: (element: any, arg: Arg) => R,
      arg?: Arg,
      options?: { timeout?: number },
    ): Promise<Awaited<R>>
    /** Also writes the image to `path`; without `type`, its extension picks PNG or JPEG. */
    screenshot(options?: Omit<ScreenshotOptions, 'fullPage' | 'clip'> & { path?: string }): Promise<Buffer>
  }
}
