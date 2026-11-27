#!/usr/bin/env node
/**
 * Fluxwright RAG Crawler Starter
 *
 * Starts an asynchronous recursive crawl via `POST /v1/crawl` (`mode: "auto"`,
 * CSS `extract` rules, `formats: ["markdown"]`), polls `GET /v1/crawl/{id}`
 * until completion, and emits clean JSONL chunks ready for vector ingestion.
 */

import { createHash } from 'node:crypto'
import { writeFile } from 'node:fs/promises'
import { setTimeout as sleep } from 'node:timers/promises'

const SERVER_URL = (process.env.FLUXWRIGHT_SERVER_URL ?? 'http://127.0.0.1:7331').replace(/\/+$/, '')
const SERVER_TOKEN = process.env.FLUXWRIGHT_SERVER_TOKEN?.trim()
const SEED_URL = process.argv[2] ?? process.env.CRAWL_URL ?? 'https://example.com'
const OUT_FILE = process.env.OUT_FILE?.trim() || ''

const MAX_DEPTH = Number(process.env.CRAWL_MAX_DEPTH ?? 2)
const MAX_PAGES = Number(process.env.CRAWL_MAX_PAGES ?? 25)
const CONCURRENCY = Number(process.env.CRAWL_CONCURRENCY ?? 4)
const CHUNK_SIZE = Number(process.env.CHUNK_SIZE ?? 1000)
const CHUNK_OVERLAP = Number(process.env.CHUNK_OVERLAP ?? 150)
const POLL_INTERVAL_MS = Number(process.env.POLL_INTERVAL_MS ?? 500)

if (!SERVER_TOKEN) {
  console.error('Error: FLUXWRIGHT_SERVER_TOKEN environment variable is required.')
  process.exit(1)
}

async function api(path, init = {}) {
  const res = await fetch(`${SERVER_URL}${path}`, {
    ...init,
    headers: {
      Authorization: `Bearer ${SERVER_TOKEN}`,
      ...(init.body ? { 'Content-Type': 'application/json' } : {}),
      ...init.headers,
    },
  })
  const text = await res.text()
  let body = null
  try {
    body = text ? JSON.parse(text) : null
  } catch {
    body = text
  }
  if (!res.ok) {
    throw new Error(`${init.method ?? 'GET'} ${path} failed (${res.status}): ${text}`)
  }
  return body
}

/** Split markdown into clean overlapping chunks respecting paragraph boundaries. */
function chunkMarkdown(markdown, maxChars = CHUNK_SIZE, overlap = CHUNK_OVERLAP) {
  const paragraphs = markdown
    .split(/\n{2,}/)
    .map((p) => p.trim())
    .filter(Boolean)

  const chunks = []
  let current = ''

  for (const para of paragraphs) {
    if (!current) {
      current = para
      continue
    }
    if (current.length + 2 + para.length <= maxChars) {
      current += `\n\n${para}`
      continue
    }
    chunks.push(current)
    const tail = overlap > 0 ? current.slice(-overlap).trimStart() : ''
    current = tail ? `${tail}\n\n${para}` : para
  }
  if (current) chunks.push(current)
  return chunks
}

function makeChunkId(url, chunkIndex) {
  return createHash('sha256').update(`${url}#${chunkIndex}`).digest('hex').slice(0, 16)
}

let activeCrawlId = null

process.once('SIGINT', async () => {
  if (activeCrawlId) {
    console.error(`\nCancelling crawl ${activeCrawlId}...`)
    try {
      await api(`/v1/crawl/${activeCrawlId}`, { method: 'DELETE' })
    } catch {
      // Best-effort cancellation on shutdown.
    }
  }
  process.exit(130)
})

async function main() {
  const payload = {
    url: SEED_URL,
    mode: 'auto',
    max_depth: MAX_DEPTH,
    max_pages: MAX_PAGES,
    concurrency: CONCURRENCY,
    same_origin: true,
    formats: ['markdown'],
    extract: {
      heading: 'h1',
      description: { selector: 'meta[name="description"]', attr: 'content' },
      sections: { selector: 'h2, h3', all: true },
    },
  }

  console.error(`[rag-crawler] Starting crawl of ${SEED_URL} (max_depth=${MAX_DEPTH}, max_pages=${MAX_PAGES})`)
  const created = await api('/v1/crawl', {
    method: 'POST',
    body: JSON.stringify(payload),
  })
  activeCrawlId = created.id
  console.error(`[rag-crawler] Crawl accepted: id=${created.id}`)

  let snapshot = created
  while (!['completed', 'failed', 'cancelled'].includes(snapshot.status)) {
    await sleep(POLL_INTERVAL_MS)
    snapshot = await api(`/v1/crawl/${created.id}`)
    console.error(
      `[rag-crawler] status=${snapshot.status} discovered=${snapshot.discovered} completed=${snapshot.completed} failed=${snapshot.failed}`,
    )
  }
  activeCrawlId = null

  if (snapshot.status === 'failed') {
    throw new Error(`Crawl failed: ${snapshot.error ?? 'unknown error'}`)
  }

  const lines = []
  for (const page of snapshot.pages ?? []) {
    if (page.status !== 'ok' || !page.markdown) continue
    const chunks = chunkMarkdown(page.markdown)
    for (let i = 0; i < chunks.length; i++) {
      lines.push(
        JSON.stringify({
          id: makeChunkId(page.url, i),
          url: page.url,
          title: page.title,
          chunk_index: i,
          chunk_count: chunks.length,
          text: chunks[i],
          metadata: {
            depth: page.depth,
            engine: page.engine,
            extracted: page.extracted ?? {},
          },
        }),
      )
    }
  }

  const output = lines.length ? `${lines.join('\n')}\n` : ''
  if (OUT_FILE) {
    await writeFile(OUT_FILE, output, 'utf8')
    console.error(`[rag-crawler] Wrote ${lines.length} chunks to ${OUT_FILE}`)
  } else {
    process.stdout.write(output)
    console.error(`[rag-crawler] Emitted ${lines.length} JSONL chunks to stdout`)
  }
}

main().catch((err) => {
  console.error(`[rag-crawler] Fatal: ${err.message}`)
  process.exit(1)
})
