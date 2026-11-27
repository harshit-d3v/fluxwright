export type FetchMode = 'auto' | 'http' | 'browser'
export type NavigationWait = 'load' | 'domcontentloaded' | 'networkidle' | 'commit'
export type JobPriority = 'low' | 'normal' | 'high'
export type ServerResourcePreset = 'scraping' | 'minimal' | 'no_media' | 'no-media'
export type ServerResourceType = 'image' | 'font' | 'media' | 'stylesheet' | 'script'
export type ScrapeFormat = 'markdown' | 'html' | 'text' | 'snapshot' | 'screenshot' | 'pdf'
export type CrawlStatus = 'queued' | 'running' | 'completed' | 'failed' | 'cancelled'
export type JobStatus = 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled'
export type LiveViewMode = 'read_only' | 'interactive'

export interface ExtractFieldSpec {
  selector: string
  attr?: string
  all?: boolean
}

export type ExtractField = string | ExtractFieldSpec

export interface FluxwrightClientOptions {
  baseUrl: string
  token: string
  timeoutMs?: number
  fetch?: typeof globalThis.fetch
}

export interface PollOptions {
  pollIntervalMs?: number
  timeoutMs?: number
}

export interface HealthzResponse {
  status: string
}

export interface ScrapeRequest {
  url: string
  formats?: Array<ScrapeFormat>
  mode?: FetchMode
  wait_until?: NavigationWait
  wait_for_selector?: string
  timeout_ms?: number
  priority?: JobPriority
  resource_preset?: ServerResourcePreset
  block_resources?: Array<ServerResourceType>
  block_url_patterns?: Array<string>
  block_cookie_banners?: boolean
  profile?: string
  persist_profile?: boolean
  proxy_session?: string
  screenshot_full_page?: boolean
  screenshot_annotate?: boolean
  snapshot_interactive_only?: boolean
  extract?: Record<string, ExtractField>
}

export interface ScrapeResponse {
  url: string
  title: string
  duration_ms: number
  engine: string
  fallback_reason?: string
  markdown?: string
  html?: string
  text?: string
  snapshot?: unknown
  screenshot_base64?: string
  pdf_base64?: string
  extracted?: Record<string, unknown>
}

export interface MapRequest {
  url: string
  same_origin?: boolean
  include_subdomains?: boolean
  include_sitemap?: boolean
  mode?: FetchMode
  search?: string
  limit?: number
  wait_until?: NavigationWait
  timeout_ms?: number
  resource_preset?: ServerResourcePreset
  block_cookie_banners?: boolean
  profile?: string
  proxy_session?: string
}

export interface MapResponse {
  url: string
  count: number
  links: Array<string>
  duration_ms: number
  engine: string
  fallback_reason?: string
}

export interface CrawlRequest {
  url: string
  max_depth?: number
  max_pages?: number
  concurrency?: number
  same_origin?: boolean
  include_subdomains?: boolean
  include_paths?: Array<string>
  exclude_paths?: Array<string>
  formats?: Array<ScrapeFormat>
  mode?: FetchMode
  extract?: Record<string, ExtractField>
  timeout_ms?: number
  wait_until?: NavigationWait
  resource_preset?: ServerResourcePreset
  block_cookie_banners?: boolean
  profile?: string
  proxy_session?: string
}

export interface CrawlPageResult {
  url: string
  depth: number
  status: string
  engine: string
  title: string
  markdown?: string
  html?: string
  text?: string
  extracted?: Record<string, unknown>
  duration_ms: number
  error?: string
}

export interface CrawlSnapshot {
  id: string
  status: CrawlStatus
  seed_url: string
  created_at: number
  started_at: number | null
  finished_at: number | null
  discovered: number
  completed: number
  failed: number
  pages: Array<CrawlPageResult>
  error?: string
}

export interface JobStep {
  action: string
  [key: string]: unknown
}

export interface JobRequest {
  steps: Array<JobStep>
  timeout_ms?: number
  retries?: number
  priority?: JobPriority
  resource_preset?: ServerResourcePreset
  block_resources?: Array<ServerResourceType>
  block_url_patterns?: Array<string>
  block_cookie_banners?: boolean
  profile?: string
  persist_profile?: boolean
  proxy_session?: string
  webhook_url?: string
  webhook_headers?: Record<string, string>
}

export interface FailureEvidence {
  console?: Array<{ kind: string; text: string }>
  network?: Array<{ url: string; method: string; status?: number; resource_type?: string; failure?: string }>
  screenshot?: string
}

export interface JobSnapshot {
  id: string
  status: JobStatus
  submitted_at: number
  started_at: number | null
  finished_at: number | null
  attempts: number
  step_index: number | null
  results: Array<unknown> | null
  error: string | null
  trace_id?: string
  failure_evidence?: FailureEvidence
}

export interface CreateSessionRequest {
  ttl_secs?: number
  keep_alive?: boolean
  priority?: JobPriority
  resource_preset?: ServerResourcePreset
  block_cookie_banners?: boolean
  profile?: string
  persist_profile?: boolean
  proxy_session?: string
  viewport?: { width: number; height: number }
  live_view_mode?: LiveViewMode
  record_frames?: boolean
  cdp_endpoint?: string
}

export interface ManagedSession {
  id: string
  status: string
  cdp_path: string
  cdp_ws_url?: string
  live_view_url?: string
  live_view_mode?: LiveViewMode
  keep_alive: boolean
  connected: boolean
  url: string
  profile: string | null
  proxy_session: string | null
  record_frames: boolean
  byob: boolean
  created_at: number
  expires_at: number
  ttl_secs?: number
}

export interface ListSessionsResponse {
  sessions: Array<ManagedSession>
}

export interface DeleteSessionResponse {
  id: string
  status: string
}

export interface SessionFileMetadata {
  name: string
  size?: number
  created_at?: number
  updated_at?: number
  [key: string]: unknown
}

export interface ListSessionFilesResponse {
  files: Array<SessionFileMetadata>
  [key: string]: unknown
}

export interface UploadSessionFileResponse {
  name: string
  size?: number
  [key: string]: unknown
}

export interface DeleteSessionFileResponse {
  deleted?: boolean
  name?: string
  [key: string]: unknown
}

export class FluxwrightApiError extends Error {
  readonly status: number
  readonly code: string | null
  readonly body: unknown

  constructor(message: string, options?: { status: number; code?: string | null; body?: unknown })
}

export class FluxwrightClient {
  readonly baseUrl: string

  constructor(options: FluxwrightClientOptions)

  healthz(): Promise<HealthzResponse>
  openapi(): Promise<Record<string, unknown>>
  stats(): Promise<Record<string, unknown>>
  metrics(): Promise<string>

  scrape(request: ScrapeRequest): Promise<ScrapeResponse>
  map(request: MapRequest): Promise<MapResponse>

  startCrawl(request: CrawlRequest): Promise<CrawlSnapshot>
  getCrawl(id: string): Promise<CrawlSnapshot>
  cancelCrawl(id: string): Promise<CrawlSnapshot>
  crawl(request: CrawlRequest, options?: PollOptions): Promise<CrawlSnapshot>

  submitJob(request: JobRequest): Promise<JobSnapshot>
  getJob(id: string): Promise<JobSnapshot>
  cancelJob(id: string): Promise<JobSnapshot>
  runJob(request: JobRequest, options?: PollOptions): Promise<JobSnapshot>

  createSession(request?: CreateSessionRequest): Promise<ManagedSession>
  listSessions(): Promise<ListSessionsResponse>
  getSession(id: string): Promise<ManagedSession>
  extendSession(id: string, ttlSecs?: number): Promise<ManagedSession>
  deleteSession(id: string): Promise<DeleteSessionResponse>

  listSessionFiles(id: string): Promise<ListSessionFilesResponse>
  uploadSessionFile(id: string, name: string, content: Buffer | Uint8Array | string): Promise<UploadSessionFileResponse>
  downloadSessionFile(id: string, name: string): Promise<Buffer>
  deleteSessionFile(id: string, name: string): Promise<DeleteSessionFileResponse>
}
