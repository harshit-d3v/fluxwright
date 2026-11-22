import type {
  Browser,
  BrowserContext,
  ExpectOptions,
  LaunchOptions,
  Locator,
  LocatorAssertions,
  NewPageOptions,
  Page,
  PageAssertions,
} from './addon'

export type {
  Browser,
  BrowserContext,
  ExpectOptions,
  LaunchOptions,
  Locator,
  LocatorAssertions,
  NewPageOptions,
  Page,
  PageAssertions,
}

export type ScreenshotMode = 'off' | 'on' | 'only-on-failure'
export type TraceMode = 'off' | 'on' | 'retain-on-failure'
export type VideoMode = 'off' | 'on' | 'retain-on-failure'

export interface TestUseOptions extends NewPageOptions {
  headless?: boolean
  maxBrowsers?: number
  launchOptions?: LaunchOptions
  screenshot?: ScreenshotMode
  trace?: TraceMode
  video?: VideoMode
  timeout?: number
  actionTimeout?: number
  navigationTimeout?: number
}

export interface TestFixtures {
  page: Page
  context: BrowserContext
  browser: Browser
  baseURL: string | undefined
}

export interface TestAttachment {
  name: string
  contentType: string
  path?: string
  body?: Buffer
}

export interface TestInfo {
  readonly title: string
  readonly titlePath: string[]
  readonly file: string
  readonly retry: number
  readonly outputDir: string
  outputPath(...segments: string[]): string
  status: 'passed' | 'failed' | 'skipped' | 'timedOut'
  error?: Error
  attachments: TestAttachment[]
  attach(
    name: string,
    options?: {
      path?: string
      body?: string | Buffer
      contentType?: string
    },
  ): Promise<void>
}

export type TestFn = (fixtures: TestFixtures, testInfo: TestInfo) => unknown
export type SuiteHookFn = (fixtures: { browser: Browser | null }) => unknown

export interface DescribeFn {
  (title: string, fn: () => void): void
  only(title: string, fn: () => void): void
  skip(title: string, fn: () => void): void
}

export interface TestType {
  (title: string, fn: TestFn): void
  only(title: string, fn: TestFn): void
  skip(title: string, fn?: TestFn): void
  skip(condition?: boolean, reason?: string): void
  describe: DescribeFn
  beforeEach(fn: TestFn): void
  afterEach(fn: TestFn): void
  beforeAll(fn: SuiteHookFn): void
  afterAll(fn: SuiteHookFn): void
  use(options: TestUseOptions): void
  info(): TestInfo
  readonly expect: Expect
}

export interface ValueAssertions<T> {
  readonly not: ValueAssertions<T>
  toBe(expected: T): void
  toEqual(expected: unknown): void
  toStrictEqual(expected: unknown): void
  toContain(expected: unknown): void
  toMatch(expected: string | RegExp): void
  toBeTruthy(): void
  toBeFalsy(): void
  toBeNull(): void
  toBeUndefined(): void
  toBeDefined(): void
  toBeGreaterThan(expected: number): void
  toBeGreaterThanOrEqual(expected: number): void
  toBeLessThan(expected: number): void
  toBeLessThanOrEqual(expected: number): void
  toBeCloseTo(expected: number, numDigits?: number): void
  toHaveLength(expected: number): void
  toHaveProperty(keyPath: string | Array<string>, value?: unknown): void
  toThrow(expected?: string | RegExp | (new (...args: any[]) => Error)): void
}

export interface Expect {
  (page: Page): PageAssertions
  (locator: Locator): LocatorAssertions
  <T>(actual: T): ValueAssertions<T>
  configure(options: ExpectOptions): Expect
}

export interface FluxwrightFixturesOptions {
  launchOptions?: LaunchOptions
  contextOptions?: NewPageOptions
  maxBrowsers?: number
  headless?: boolean
  baseURL?: string
  viewport?: { width: number; height: number } | null
  screenshot?: ScreenshotMode
  trace?: TraceMode
  video?: VideoMode
  outputDir?: string
}

export interface AcquiredFluxwrightFixtures {
  browser: Browser
  context: BrowserContext
  page: Page
  dispose(failed?: boolean): Promise<void>
}

export interface NodeTestContextLike {
  readonly name?: string
  after(fn: () => unknown): void
}

export interface VitestFluxwrightFixtures {
  browser: (fixtures: Record<string, never>, use: (browser: Browser) => Promise<void>) => Promise<void>
  context: (fixtures: { browser: Browser }, use: (context: BrowserContext) => Promise<void>) => Promise<void>
  page: (fixtures: { context: BrowserContext }, use: (page: Page) => Promise<void>) => Promise<void>
}

export interface FluxwrightFixtures {
  getBrowser(): Promise<Browser>
  setup(): Promise<Browser>
  close(): Promise<void>
  teardown(): Promise<void>
  acquire(testContextOrOptions?: NodeTestContextLike | (NewPageOptions & { name?: string })): Promise<AcquiredFluxwrightFixtures>
  withPage<T>(
    fn: (fixtures: { browser: Browser; context: BrowserContext; page: Page }) => Promise<T> | T,
    testContextOrOptions?: NodeTestContextLike | (NewPageOptions & { name?: string }),
  ): Promise<T>
  readonly vitest: VitestFluxwrightFixtures
  readonly expect: Expect
  getLaunchCount(): number
}

export interface FluxwrightTestConfig {
  testDir?: string
  workers?: number
  retries?: number
  timeout?: number
  outputDir?: string
  reportDir?: string
  reporter?: Array<'list' | 'html' | 'json'> | string
  use?: TestUseOptions
}

export const test: TestType
export const expect: Expect
export function createFluxwrightFixtures(options?: FluxwrightFixturesOptions): FluxwrightFixtures
export function defineConfig(config: FluxwrightTestConfig): FluxwrightTestConfig
