#!/usr/bin/env node
'use strict'

const { fork } = require('node:child_process')
const fs = require('node:fs/promises')
const path = require('node:path')
const { writeHtmlReport, formatDuration } = require('./html_report.js')
const { _internals } = require('./test_runner.js')

const TEST_FILE_RE = /\.(?:test|spec)\.(?:js|mjs|cjs|ts|mts|cts)$/i
const IGNORE_DIRS = new Set([
  'node_modules',
  '.git',
  'target',
  'fluxwright-report',
  'test-results',
  '.venv',
  'venv',
  '__pycache__',
  'dist',
  'build',
])

function printHelp() {
  process.stdout.write(`Usage: fluxwright-test [options] [paths...]

Options:
  --workers=<N>, -j <N>        Number of parallel worker processes (default: 1)
  --shard=<k/n>                Run shard k of n (1-indexed, e.g. 1/2)
  --retries=<N>                Max retries per failed test (default: 0)
  --grep=<pattern>, -g <pat>   Only run tests matching regex/substring
  --grep-invert=<pattern>      Skip tests matching regex/substring
  --update-snapshots, -u       Set FLUXWRIGHT_UPDATE_SNAPSHOTS=1
  --reporter=<list>            Comma-separated: list,html,json (default: list)
  --output=<dir>               Artifacts directory (default: test-results)
  --report-dir=<dir>           HTML report directory (default: fluxwright-report)
  --headed                     Run browsers in headed mode
  --screenshot=<mode>          off | on | only-on-failure (default: only-on-failure)
  --trace=<mode>               off | on | retain-on-failure (default: off)
  --video=<mode>               off | on | retain-on-failure (default: off)
  --timeout=<ms>               Per-test timeout in ms (default: 30000)
  --max-browsers=<N>           Max pooled browsers per worker
  -h, --help                   Show this help message
  -v, --version                Show version
`)
}

function parseRegexPattern(pattern) {
  const slashMatch = /^\/(.+)\/([dgimsuvy]*)$/.exec(pattern)
  if (slashMatch) {
    return new RegExp(slashMatch[1], slashMatch[2].replace(/[gy]/g, ''))
  }
  return new RegExp(pattern)
}

function parseShard(raw) {
  const m = /^(\d+)\/(\d+)$/.exec(String(raw).trim())
  if (!m) throw new Error(`Invalid --shard value "${raw}": expected k/n (e.g. 1/2)`)
  const index = Number(m[1])
  const total = Number(m[2])
  if (!Number.isInteger(index) || !Number.isInteger(total) || total < 1 || index < 1 || index > total) {
    throw new Error(`Invalid --shard value "${raw}": require 1 <= k <= n and n >= 1`)
  }
  return { index, total, raw: `${index}/${total}` }
}

function parsePositiveInt(raw, flag) {
  const n = Number(raw)
  if (!Number.isInteger(n) || n < 1) {
    throw new Error(`Invalid ${flag} value "${raw}": must be an integer >= 1`)
  }
  return n
}

function parseNonNegativeInt(raw, flag) {
  const n = Number(raw)
  if (!Number.isInteger(n) || n < 0) {
    throw new Error(`Invalid ${flag} value "${raw}": must be an integer >= 0`)
  }
  return n
}

function parseEnum(raw, allowed, flag) {
  if (!allowed.includes(raw)) {
    throw new Error(`Invalid ${flag} value "${raw}": expected ${allowed.join(' | ')}`)
  }
  return raw
}

function parseArgs(argv) {
  const opts = {
    targets: [],
    workers: 1,
    shard: null,
    retries: 0,
    grep: null,
    grepInvert: null,
    updateSnapshots: false,
    reporters: ['list'],
    outputDir: 'test-results',
    reportDir: process.env.FLUXWRIGHT_HTML_REPORT || 'fluxwright-report',
    headed: false,
    screenshot: 'only-on-failure',
    trace: 'off',
    video: 'off',
    timeout: undefined,
    maxBrowsers: undefined,
    help: false,
    version: false,
  }

  let endOfFlags = false
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]
    if (endOfFlags) {
      opts.targets.push(arg)
      continue
    }
    if (arg === '--') {
      endOfFlags = true
      continue
    }
    if (arg === '-h' || arg === '--help') {
      opts.help = true
      continue
    }
    if (arg === '-v' || arg === '--version') {
      opts.version = true
      continue
    }
    if (arg === '-u' || arg === '--update-snapshots') {
      opts.updateSnapshots = true
      continue
    }
    if (arg === '--headed') {
      opts.headed = true
      continue
    }

    const eqIdx = arg.indexOf('=')
    let flag = arg
    let val
    let hasInlineVal = false
    if (eqIdx !== -1 && arg.startsWith('--')) {
      flag = arg.slice(0, eqIdx)
      val = arg.slice(eqIdx + 1)
      hasInlineVal = true
    }

    const takeValue = () => {
      if (hasInlineVal) return val
      if (i + 1 >= argv.length) throw new Error(`Missing value for ${flag}`)
      i += 1
      return argv[i]
    }

    if (flag === '--workers' || flag === '-j') {
      opts.workers = parsePositiveInt(takeValue(), flag)
    } else if (flag === '--shard') {
      opts.shard = parseShard(takeValue())
    } else if (flag === '--retries') {
      opts.retries = parseNonNegativeInt(takeValue(), flag)
    } else if (flag === '--grep' || flag === '-g') {
      opts.grep = parseRegexPattern(takeValue())
    } else if (flag === '--grep-invert') {
      opts.grepInvert = parseRegexPattern(takeValue())
    } else if (flag === '--reporter') {
      const list = takeValue().split(',').map((s) => s.trim().toLowerCase()).filter(Boolean)
      if (!list.length) throw new Error('Invalid --reporter: must specify at least one reporter')
      for (const r of list) parseEnum(r, ['list', 'html', 'json'], '--reporter')
      opts.reporters = [...new Set(list)]
    } else if (flag === '--output') {
      opts.outputDir = takeValue()
    } else if (flag === '--report-dir') {
      opts.reportDir = takeValue()
    } else if (flag === '--screenshot') {
      opts.screenshot = parseEnum(takeValue(), ['off', 'on', 'only-on-failure'], '--screenshot')
    } else if (flag === '--trace') {
      opts.trace = parseEnum(takeValue(), ['off', 'on', 'retain-on-failure'], '--trace')
    } else if (flag === '--video') {
      opts.video = parseEnum(takeValue(), ['off', 'on', 'retain-on-failure'], '--video')
    } else if (flag === '--timeout') {
      opts.timeout = parseNonNegativeInt(takeValue(), '--timeout')
    } else if (flag === '--max-browsers') {
      opts.maxBrowsers = parsePositiveInt(takeValue(), '--max-browsers')
    } else if (arg.startsWith('-')) {
      throw new Error(`Unknown option: ${arg}`)
    } else {
      opts.targets.push(arg)
    }
  }

  return opts
}

async function walkDir(dir, out) {
  const entries = await fs.readdir(dir, { withFileTypes: true })
  entries.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0))
  for (const entry of entries) {
    if (entry.isDirectory()) {
      if (IGNORE_DIRS.has(entry.name) || entry.name.startsWith('.')) continue
      await walkDir(path.join(dir, entry.name), out)
    } else if (entry.isFile() && TEST_FILE_RE.test(entry.name)) {
      out.push(path.join(dir, entry.name))
    }
  }
}

async function discoverTestFiles(targets, cwd) {
  const roots = targets.length ? targets : [cwd]
  const found = new Set()
  for (const rawTarget of roots) {
    const absTarget = path.resolve(cwd, rawTarget)
    let stat
    try {
      stat = await fs.stat(absTarget)
    } catch {
      throw new Error(`Test path does not exist: ${rawTarget}`)
    }
    if (stat.isFile()) {
      found.add(absTarget)
    } else if (stat.isDirectory()) {
      const files = []
      await walkDir(absTarget, files)
      for (const f of files) found.add(f)
    }
  }
  const relKey = (p) => path.relative(cwd, p).replace(/\\/g, '/')
  return [...found].sort((a, b) => {
    const ka = relKey(a)
    const kb = relKey(b)
    return ka < kb ? -1 : ka > kb ? 1 : 0
  })
}

function selectTests(fileEntries, opts) {
  const all = []
  for (const entry of fileEntries) {
    for (const t of entry.tests) {
      all.push({ entry, test: t })
    }
  }
  const hasOnly = all.some((x) => x.test.only)
  let filtered = hasOnly ? all.filter((x) => x.test.only) : all

  if (opts.grep) {
    filtered = filtered.filter((x) => {
      opts.grep.lastIndex = 0
      return opts.grep.test(x.test.fullTitle)
    })
  }
  if (opts.grepInvert) {
    filtered = filtered.filter((x) => {
      opts.grepInvert.lastIndex = 0
      return !opts.grepInvert.test(x.test.fullTitle)
    })
  }

  const totalAfterFilter = filtered.length
  if (opts.shard) {
    const { index, total } = opts.shard
    const start = Math.floor(((index - 1) * totalAfterFilter) / total)
    const end = Math.floor((index * totalAfterFilter) / total)
    filtered = filtered.slice(start, end)
  }

  return { selected: filtered, totalBeforeShard: totalAfterFilter }
}

function groupByFile(selected) {
  const map = new Map()
  for (const item of selected) {
    let bucket = map.get(item.entry.absPath)
    if (!bucket) {
      bucket = { entry: item.entry, absPath: item.entry.absPath, selectedIds: [] }
      map.set(item.entry.absPath, bucket)
    }
    bucket.selectedIds.push(item.test.id)
  }
  return [...map.values()]
}

function printLiveResult(result, index, total, enabled) {
  if (!enabled) return
  const mark =
    result.status === 'passed'
      ? '✓'
      : result.status === 'flaky'
        ? '±'
        : result.status === 'skipped'
          ? '-'
          : '✘'
  const label = result.titlePath.join(' › ')
  const suffix = result.status === 'skipped' ? ' (skipped)' : ` (${formatDuration(result.durationMs)})`
  process.stdout.write(`  ${mark} [${index}/${total}] ${label}${suffix}\n`)
}

async function runWithWorkers(tasks, workersCount, runOptions, cwd, totalSelected, listEnabled) {
  const allResults = []
  let completed = 0
  const queue = [...tasks]
  const poolCount = Math.min(workersCount, queue.length)

  async function spawnWorker() {
    return new Promise((resolve, reject) => {
      const child = fork(__filename, ['--__worker'], {
        cwd,
        env: { ...process.env },
        stdio: ['inherit', 'inherit', 'inherit', 'ipc'],
      })
      let currentPromiseResolve = null
      let currentPromiseReject = null

      child.on('message', async (msg) => {
        if (!msg || typeof msg !== 'object') return
        if (msg.type === 'result') {
          completed += 1
          allResults.push(msg.result)
          printLiveResult(msg.result, completed, totalSelected, listEnabled)
        } else if (msg.type === 'task-done') {
          if (currentPromiseResolve) {
            const res = currentPromiseResolve
            currentPromiseResolve = null
            currentPromiseReject = null
            res()
          }
        } else if (msg.type === 'task-error') {
          if (currentPromiseReject) {
            const rej = currentPromiseReject
            currentPromiseResolve = null
            currentPromiseReject = null
            rej(new Error(msg.error || 'Worker task failed'))
          }
        }
      })

      child.on('error', (err) => {
        if (currentPromiseReject) currentPromiseReject(err)
        reject(err)
      })

      child.on('exit', (code) => {
        if (code && code !== 0) {
          const err = new Error(`Worker exited with code ${code}`)
          if (currentPromiseReject) currentPromiseReject(err)
          reject(err)
        } else {
          resolve()
        }
      })

      ;(async () => {
        try {
          while (queue.length > 0) {
            const task = queue.shift()
            await new Promise((res, rej) => {
              currentPromiseResolve = res
              currentPromiseReject = rej
              child.send({
                type: 'run',
                absPath: task.absPath,
                selectedIds: task.selectedIds,
                runOptions,
                cwd,
              })
            })
          }
          child.send({ type: 'shutdown' })
        } catch (err) {
          try {
            child.kill()
          } catch {}
          reject(err)
        }
      })()
    })
  }

  await Promise.all(Array.from({ length: poolCount }, () => spawnWorker()))
  return allResults
}

async function runWorkerProcess() {
  let pool = null
  process.on('message', async (msg) => {
    if (!msg || typeof msg !== 'object') return
    if (msg.type === 'run') {
      try {
        if (!pool) {
          pool = _internals.createBrowserPool({
            headless: msg.runOptions.headless,
            maxBrowsers: msg.runOptions.maxBrowsers,
          })
        }
        const entry = await _internals.loadTestFile(msg.absPath, msg.cwd)
        await _internals.runTestsForFile(entry, msg.selectedIds, pool, msg.runOptions, (res) => {
          process.send({ type: 'result', result: res })
        })
        process.send({ type: 'task-done' })
      } catch (err) {
        process.send({ type: 'task-error', error: err instanceof Error ? err.stack || err.message : String(err) })
      }
    } else if (msg.type === 'shutdown') {
      if (pool) await pool.closeAll()
      process.exit(0)
    }
  })
}

async function runCli(argv = process.argv.slice(2), cwd = process.cwd()) {
  if (argv.includes('--__worker')) {
    await runWorkerProcess()
    return 0
  }

  let opts
  try {
    opts = parseArgs(argv)
  } catch (err) {
    process.stderr.write(`Error: ${err.message}\n`)
    return 1
  }

  if (opts.help) {
    printHelp()
    return 0
  }
  if (opts.version) {
    const pkg = require('./package.json')
    process.stdout.write(`${pkg.version}\n`)
    return 0
  }

  if (opts.updateSnapshots) {
    process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS = '1'
  }

  const startedAt = new Date().toISOString()
  const startMs = Date.now()

  let files
  try {
    files = await discoverTestFiles(opts.targets, cwd)
  } catch (err) {
    process.stderr.write(`Error: ${err.message}\n`)
    return 1
  }

  if (files.length === 0) {
    process.stderr.write('No test files found.\n')
    return 1
  }

  const fileEntries = []
  for (const file of files) {
    fileEntries.push(await _internals.loadTestFile(file, cwd))
  }

  const { selected } = selectTests(fileEntries, opts)
  const tasks = groupByFile(selected)
  const listEnabled = opts.reporters.includes('list')

  const runOptions = {
    retries: opts.retries,
    outputDir: path.resolve(cwd, opts.outputDir),
    screenshot: opts.screenshot,
    trace: opts.trace,
    video: opts.video,
    headless: !opts.headed,
    ...(opts.timeout !== undefined ? { timeout: opts.timeout } : {}),
    ...(opts.maxBrowsers !== undefined ? { maxBrowsers: opts.maxBrowsers } : {}),
  }

  let results = []
  if (tasks.length > 0) {
    if (opts.workers > 1 && tasks.length > 1) {
      results = await runWithWorkers(tasks, opts.workers, runOptions, cwd, selected.length, listEnabled)
    } else {
      const pool = _internals.createBrowserPool({
        headless: runOptions.headless,
        maxBrowsers: runOptions.maxBrowsers,
      })
      let completed = 0
      try {
        for (const task of tasks) {
          const fileResults = await _internals.runTestsForFile(
            task.entry,
            task.selectedIds,
            pool,
            runOptions,
            (res) => {
              completed += 1
              printLiveResult(res, completed, selected.length, listEnabled)
            },
          )
          results.push(...fileResults)
        }
      } finally {
        await pool.closeAll()
      }
    }
  }

  const durationMs = Date.now() - startMs
  const passedCount = results.filter((r) => r.status === 'passed').length
  const flakyCount = results.filter((r) => r.status === 'flaky').length
  const failedCount = results.filter((r) => r.status === 'failed' || r.status === 'timedOut').length
  const skippedCount = results.filter((r) => r.status === 'skipped').length

  const report = {
    startedAt,
    durationMs,
    workers: opts.workers,
    shard: opts.shard ? opts.shard.raw : null,
    results,
  }

  if (listEnabled) {
    const failures = results.filter((r) => r.status === 'failed' || r.status === 'timedOut')
    if (failures.length > 0) {
      process.stdout.write('\nFailures:\n')
      failures.forEach((f, idx) => {
        process.stdout.write(`\n  ${idx + 1}) ${f.titlePath.join(' › ')}\n`)
        if (f.error) {
          process.stdout.write(`     ${f.error.stack || f.error.message}\n`)
        }
        for (const att of f.attachments || []) {
          if (att.path) process.stdout.write(`     attachment (${att.name}): ${att.path}\n`)
        }
      })
    }
    const parts = [
      `${passedCount} passed`,
      flakyCount ? `${flakyCount} flaky` : null,
      failedCount ? `${failedCount} failed` : null,
      skippedCount ? `${skippedCount} skipped` : null,
    ].filter(Boolean)
    process.stdout.write(`\n${parts.join(', ')} (${formatDuration(durationMs)})\n`)
  }

  if (opts.reporters.includes('html')) {
    const reportPath = opts.reportDir.endsWith('.html')
      ? path.resolve(cwd, opts.reportDir)
      : path.resolve(cwd, opts.reportDir, 'index.html')
    const written = await writeHtmlReport(report, reportPath)
    if (listEnabled) {
      process.stdout.write(`HTML report: ${written}\n`)
    }
  }

  if (opts.reporters.includes('json')) {
    process.stdout.write(`${JSON.stringify(report, null, 2)}\n`)
  }

  return failedCount > 0 ? 1 : 0
}

if (require.main === module) {
  runCli().then(
    (code) => {
      if (!process.argv.includes('--__worker')) process.exit(code)
    },
    (err) => {
      process.stderr.write(`${err instanceof Error ? err.stack || err.message : String(err)}\n`)
      process.exit(1)
    },
  )
}

module.exports = {
  discoverTestFiles,
  parseArgs,
  runCli,
  selectTests,
}
