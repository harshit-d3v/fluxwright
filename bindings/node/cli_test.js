#!/usr/bin/env node
'use strict'

const { fork, spawnSync } = require('node:child_process')
const fs = require('node:fs/promises')
const path = require('node:path')
const { writeHtmlReport, formatDuration } = require('./html_report.js')
const { _internals } = require('./test_runner.js')

const TEST_FILE_RE = /\.(?:test|spec)\.(?:js|mjs|cjs|ts|mts|cts)$/i
const CONFIG_FILENAMES = [
  'fluxwright.config.mjs',
  'fluxwright.config.js',
  'fluxwright.config.cjs',
  'fluxwright.config.ts',
]
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
  -c, --config=<path>          Path to fluxwright.config.{mjs,js,cjs,ts}
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

function compileGrepMatcher(pattern, flag = '--grep') {
  const raw = String(pattern)
  let source = raw
  let flags = ''
  if (raw.length >= 2 && raw.startsWith('/')) {
    const lastSlash = raw.lastIndexOf('/')
    if (lastSlash > 0) {
      const candidateFlags = raw.slice(lastSlash + 1)
      if (/^[dgimsuvy]*$/.test(candidateFlags)) {
        source = raw.slice(1, lastSlash)
        flags = candidateFlags.replace(/g/g, '')
      }
    }
  }
  try {
    return new RegExp(source, flags)
  } catch (err) {
    throw new Error(`Invalid ${flag} regex "${raw}": ${err.message}`)
  }
}

function normalizeReporters(raw, label) {
  let list
  if (typeof raw === 'string') {
    list = raw.split(',').map((s) => s.trim().toLowerCase()).filter(Boolean)
  } else if (Array.isArray(raw)) {
    list = raw.map((s) => String(s).trim().toLowerCase()).filter(Boolean)
  } else {
    throw new Error(`Invalid ${label}: expected string or string[]`)
  }
  if (!list.length) throw new Error(`Invalid ${label}: must specify at least one reporter`)
  for (const r of list) parseEnum(r, ['list', 'html', 'json'], label)
  return [...new Set(list)]
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
    config: undefined,
    workers: undefined,
    shard: null,
    retries: undefined,
    grep: null,
    grepInvert: null,
    updateSnapshots: false,
    reporters: undefined,
    outputDir: undefined,
    reportDir: undefined,
    headed: undefined,
    screenshot: undefined,
    trace: undefined,
    video: undefined,
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

    if (flag === '--config' || flag === '-c') {
      opts.config = takeValue()
    } else if (flag === '--workers' || flag === '-j') {
      opts.workers = parsePositiveInt(takeValue(), flag)
    } else if (flag === '--shard') {
      opts.shard = parseShard(takeValue())
    } else if (flag === '--retries') {
      opts.retries = parseNonNegativeInt(takeValue(), flag)
    } else if (flag === '--grep' || flag === '-g') {
      opts.grep = compileGrepMatcher(takeValue(), flag)
    } else if (flag === '--grep-invert') {
      opts.grepInvert = compileGrepMatcher(takeValue(), flag)
    } else if (flag === '--reporter') {
      opts.reporters = normalizeReporters(takeValue(), '--reporter')
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

async function findConfigPath(cwd, explicitPath) {
  if (explicitPath !== undefined) {
    const abs = path.resolve(cwd, explicitPath)
    let stat
    try {
      stat = await fs.stat(abs)
    } catch {
      throw new Error(`Config file does not exist: ${explicitPath}`)
    }
    if (!stat.isFile()) {
      throw new Error(`Config path is not a file: ${explicitPath}`)
    }
    return abs
  }
  for (const name of CONFIG_FILENAMES) {
    const candidate = path.join(cwd, name)
    try {
      const stat = await fs.stat(candidate)
      if (stat.isFile()) return candidate
    } catch {}
  }
  return null
}

async function loadConfig(configPath) {
  if (!configPath) return { config: {}, configDir: null }
  const mod = await _internals.importFreshModule(configPath)
  const raw = mod?.default?.default ?? mod?.default ?? mod
  if (raw == null) return { config: {}, configDir: path.dirname(configPath) }
  if (typeof raw !== 'object' || Array.isArray(raw)) {
    throw new Error(`Invalid config in ${configPath}: default export must be an object`)
  }
  const config = {}
  if (raw.testDir !== undefined) {
    if (typeof raw.testDir !== 'string' || !raw.testDir) {
      throw new Error(`Invalid config.testDir in ${configPath}: must be a non-empty string`)
    }
    config.testDir = raw.testDir
  }
  if (raw.workers !== undefined) {
    config.workers = parsePositiveInt(raw.workers, 'config.workers')
  }
  if (raw.retries !== undefined) {
    config.retries = parseNonNegativeInt(raw.retries, 'config.retries')
  }
  if (raw.timeout !== undefined) {
    config.timeout = parseNonNegativeInt(raw.timeout, 'config.timeout')
  }
  if (raw.outputDir !== undefined) {
    if (typeof raw.outputDir !== 'string' || !raw.outputDir) {
      throw new Error(`Invalid config.outputDir in ${configPath}: must be a non-empty string`)
    }
    config.outputDir = raw.outputDir
  }
  if (raw.reportDir !== undefined) {
    if (typeof raw.reportDir !== 'string' || !raw.reportDir) {
      throw new Error(`Invalid config.reportDir in ${configPath}: must be a non-empty string`)
    }
    config.reportDir = raw.reportDir
  }
  if (raw.reporter !== undefined) {
    config.reporters = normalizeReporters(raw.reporter, 'config.reporter')
  }
  if (raw.use !== undefined) {
    if (!raw.use || typeof raw.use !== 'object' || Array.isArray(raw.use)) {
      throw new Error(`Invalid config.use in ${configPath}: must be an object`)
    }
    if (raw.use.screenshot !== undefined) {
      parseEnum(raw.use.screenshot, ['off', 'on', 'only-on-failure'], 'config.use.screenshot')
    }
    if (raw.use.trace !== undefined) {
      parseEnum(raw.use.trace, ['off', 'on', 'retain-on-failure'], 'config.use.trace')
    }
    if (raw.use.video !== undefined) {
      parseEnum(raw.use.video, ['off', 'on', 'retain-on-failure'], 'config.use.video')
    }
    config.use = { ...raw.use }
  }
  return { config, configDir: path.dirname(configPath) }
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

function printLiveResult(result, index, total, enabled, stream = process.stdout) {
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
  stream.write(`  ${mark} [${index}/${Math.max(total, index)}] ${label}${suffix}\n`)
}

function buildWorkerExecArgv(tasks) {
  const execArgv = [...process.execArgv]
  const hasTsTask = tasks.some((t) => _internals.isTypeScriptFile(t.absPath))
  if (hasTsTask && _internals.needsStripTypesFlag() && !execArgv.includes('--experimental-strip-types')) {
    execArgv.push('--experimental-strip-types')
  }
  return execArgv
}

function defaultPoolLaunchOptions(runOptions) {
  const cfgUse = runOptions?.configUse || {}
  const cliUse = runOptions?.cliUse || {}
  return {
    ...(cfgUse.launchOptions || {}),
    ...(cliUse.launchOptions || {}),
    ...(cfgUse.headless !== undefined ? { headless: cfgUse.headless } : {}),
    ...(cliUse.headless !== undefined ? { headless: cliUse.headless } : {}),
    ...(cfgUse.maxBrowsers !== undefined ? { maxBrowsers: cfgUse.maxBrowsers } : {}),
    ...(cliUse.maxBrowsers !== undefined ? { maxBrowsers: cliUse.maxBrowsers } : {}),
  }
}

async function runWithWorkers(tasks, workersCount, runOptions, cwd, totalSelected, listEnabled, listStream = process.stdout) {
  const allResults = []
  let completed = 0
  const queue = [...tasks]
  const poolCount = Math.min(workersCount, queue.length)
  const workerExecArgv = buildWorkerExecArgv(tasks)
  const activeWorkers = new Set()
  let firstError = null
  let aborting = false

  function abortSiblings(exceptEntry) {
    aborting = true
    queue.length = 0
    for (const entry of activeWorkers) {
      if (entry === exceptEntry) continue
      entry.abort()
    }
  }

  function spawnWorker() {
    return new Promise((resolve, reject) => {
      const child = fork(__filename, ['--__worker'], {
        cwd,
        env: { ...process.env },
        execArgv: workerExecArgv,
        stdio: ['inherit', 'inherit', 'inherit', 'ipc'],
      })
      let currentPromiseResolve = null
      let currentPromiseReject = null
      let normalShutdown = false
      let killTimer = null
      let settled = false

      const finishResolve = () => {
        if (settled) return
        settled = true
        if (killTimer) {
          clearTimeout(killTimer)
          killTimer = null
        }
        resolve()
      }

      const finishReject = (err) => {
        if (!firstError) firstError = err
        abortSiblings(entry)
        if (settled) return
        settled = true
        if (killTimer) {
          clearTimeout(killTimer)
          killTimer = null
        }
        reject(err)
      }

      const entry = {
        child,
        abort() {
          if (currentPromiseReject) {
            const rej = currentPromiseReject
            currentPromiseResolve = null
            currentPromiseReject = null
            rej(new Error('Worker aborted due to sibling failure'))
          }
          if (child.exitCode !== null || child.signalCode !== null) return
          try {
            if (child.connected) child.send({ type: 'abort' })
          } catch {}
          if (process.platform !== 'win32') {
            try {
              child.kill('SIGTERM')
            } catch {}
          }
          if (!killTimer) {
            killTimer = setTimeout(() => {
              try {
                if (child.exitCode === null && child.signalCode === null) {
                  child.kill('SIGKILL')
                }
              } catch {
                try {
                  child.kill()
                } catch {}
              }
            }, 3000)
            if (typeof killTimer.unref === 'function') killTimer.unref()
          }
        },
      }
      activeWorkers.add(entry)

      child.on('message', (msg) => {
        if (!msg || typeof msg !== 'object') return
        if (msg.type === 'result') {
          completed += 1
          allResults.push(msg.result)
          printLiveResult(msg.result, completed, totalSelected, listEnabled, listStream)
        } else if (msg.type === 'task-done') {
          if (currentPromiseResolve) {
            const res = currentPromiseResolve
            currentPromiseResolve = null
            currentPromiseReject = null
            res()
          }
        } else if (msg.type === 'task-error') {
          const err = new Error(msg.error || 'Worker task failed')
          if (currentPromiseReject) {
            const rej = currentPromiseReject
            currentPromiseResolve = null
            currentPromiseReject = null
            rej(err)
          } else {
            finishReject(err)
          }
        }
      })

      child.on('error', (err) => {
        if (currentPromiseReject) {
          const rej = currentPromiseReject
          currentPromiseResolve = null
          currentPromiseReject = null
          rej(err)
        }
        finishReject(err)
      })

      child.on('exit', (code, signal) => {
        activeWorkers.delete(entry)
        if (killTimer) {
          clearTimeout(killTimer)
          killTimer = null
        }
        if (aborting) {
          if (currentPromiseReject) {
            const rej = currentPromiseReject
            currentPromiseResolve = null
            currentPromiseReject = null
            rej(firstError || new Error('Worker aborted'))
          }
          finishResolve()
          return
        }
        if (signal != null || code !== 0) {
          const err =
            signal != null
              ? new Error(`Worker terminated by signal ${signal}`)
              : new Error(`Worker exited with code ${code}`)
          if (currentPromiseReject) {
            const rej = currentPromiseReject
            currentPromiseResolve = null
            currentPromiseReject = null
            rej(err)
          }
          finishReject(err)
          return
        }
        if (!normalShutdown) {
          const err = new Error('Worker exited unexpectedly before shutdown')
          if (currentPromiseReject) {
            const rej = currentPromiseReject
            currentPromiseResolve = null
            currentPromiseReject = null
            rej(err)
          }
          finishReject(err)
          return
        }
        finishResolve()
      })

      ;(async () => {
        try {
          while (!aborting && queue.length > 0) {
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
          if (!aborting) {
            normalShutdown = true
            if (child.connected) child.send({ type: 'shutdown' })
          }
        } catch (err) {
          if (!aborting) {
            if (!firstError) firstError = err
            abortSiblings(entry)
            entry.abort()
            if (child.exitCode !== null || child.signalCode !== null) {
              finishReject(err)
            }
          }
        }
      })()
    })
  }

  await Promise.allSettled(Array.from({ length: poolCount }, () => spawnWorker()))
  if (firstError) throw firstError
  return allResults
}

async function runWorkerProcess() {
  let pool = null
  let shuttingDown = false

  const cleanupAndExit = async (code) => {
    if (shuttingDown) return
    shuttingDown = true
    try {
      if (pool) await pool.closeAll()
    } catch {}
    process.exit(code)
  }

  process.on('disconnect', () => {
    cleanupAndExit(1)
  })
  process.on('SIGINT', () => {
    cleanupAndExit(130)
  })
  process.on('SIGTERM', () => {
    cleanupAndExit(143)
  })

  process.on('message', async (msg) => {
    if (!msg || typeof msg !== 'object') return
    if (msg.type === 'run') {
      try {
        if (!pool) {
          pool = _internals.createBrowserPool(defaultPoolLaunchOptions(msg.runOptions))
        }
        const entry = await _internals.loadTestFile(msg.absPath, msg.cwd)
        await _internals.runTestsForFile(entry, msg.selectedIds, pool, msg.runOptions, (res) => {
          if (process.connected) process.send({ type: 'result', result: res })
        })
        if (process.connected) process.send({ type: 'task-done' })
      } catch (err) {
        if (process.connected) {
          process.send({ type: 'task-error', error: err instanceof Error ? err.stack || err.message : String(err) })
        }
      }
    } else if (msg.type === 'shutdown') {
      await cleanupAndExit(0)
    } else if (msg.type === 'abort') {
      await cleanupAndExit(1)
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

  let configPath
  try {
    configPath = await findConfigPath(cwd, opts.config)
  } catch (err) {
    process.stderr.write(`Error: ${err.message}\n`)
    return 1
  }

  // On Node 22.6-22.17 invoked directly via CLI, re-exec with --experimental-strip-types
  // if config or explicit targets include TypeScript files.
  if (
    require.main === module &&
    _internals.needsStripTypesFlag() &&
    !process.env.FLUXWRIGHT_TS_REEXEC &&
    ((configPath && _internals.isTypeScriptFile(configPath)) ||
      opts.targets.some((t) => _internals.isTypeScriptFile(t)))
  ) {
    const child = spawnSync(
      process.execPath,
      ['--experimental-strip-types', ...process.execArgv, __filename, ...argv],
      { cwd, stdio: 'inherit', shell: false, env: { ...process.env, FLUXWRIGHT_TS_REEXEC: '1' } },
    )
    return child.signal ? 1 : (child.status ?? 1)
  }

  let config
  let configDir
  try {
    const loaded = await loadConfig(configPath)
    config = loaded.config
    configDir = loaded.configDir || cwd
  } catch (err) {
    process.stderr.write(`Error: ${err.message}\n`)
    return 1
  }

  const workers = opts.workers ?? config.workers ?? 1
  const retries = opts.retries ?? config.retries ?? 0
  const reporters = opts.reporters ?? config.reporters ?? ['list']
  const outputDir =
    opts.outputDir !== undefined
      ? path.resolve(cwd, opts.outputDir)
      : config.outputDir !== undefined
        ? path.resolve(configDir, config.outputDir)
        : path.resolve(cwd, 'test-results')
  const reportDir =
    opts.reportDir !== undefined
      ? opts.reportDir
      : config.reportDir !== undefined
        ? path.resolve(configDir, config.reportDir)
        : process.env.FLUXWRIGHT_HTML_REPORT || 'fluxwright-report'

  const effectiveTargets =
    opts.targets.length > 0
      ? opts.targets
      : config.testDir !== undefined
        ? [path.resolve(configDir, config.testDir)]
        : []

  const hadUpdateEnv = Object.prototype.hasOwnProperty.call(process.env, 'FLUXWRIGHT_UPDATE_SNAPSHOTS')
  const prevUpdateEnv = process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
  if (opts.updateSnapshots) {
    process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS = '1'
  }

  try {
    const startedAt = new Date().toISOString()
    const startMs = Date.now()

    let files
    try {
      files = await discoverTestFiles(effectiveTargets, cwd)
    } catch (err) {
      process.stderr.write(`Error: ${err.message}\n`)
      return 1
    }

    if (files.length === 0) {
      process.stderr.write('No test files found.\n')
      return 1
    }

    if (
      require.main === module &&
      _internals.needsStripTypesFlag() &&
      !process.env.FLUXWRIGHT_TS_REEXEC &&
      files.some((f) => _internals.isTypeScriptFile(f))
    ) {
      const child = spawnSync(
        process.execPath,
        ['--experimental-strip-types', ...process.execArgv, __filename, ...argv],
        { cwd, stdio: 'inherit', shell: false, env: { ...process.env, FLUXWRIGHT_TS_REEXEC: '1' } },
      )
      return child.signal ? 1 : (child.status ?? 1)
    }

    const fileEntries = []
    for (const file of files) {
      fileEntries.push(await _internals.loadTestFile(file, cwd))
    }

    const { selected } = selectTests(fileEntries, opts)
    const tasks = groupByFile(selected)
    const listEnabled = reporters.includes('list')
    const jsonEnabled = reporters.includes('json')
    const listStream = jsonEnabled ? process.stderr : process.stdout

    const configUse = {
      ...(config.timeout !== undefined ? { timeout: config.timeout } : {}),
      ...(config.use || {}),
    }
    const cliUse = {}
    if (opts.screenshot !== undefined) cliUse.screenshot = opts.screenshot
    if (opts.trace !== undefined) cliUse.trace = opts.trace
    if (opts.video !== undefined) cliUse.video = opts.video
    if (opts.headed === true) cliUse.headless = false
    if (opts.timeout !== undefined) cliUse.timeout = opts.timeout
    if (opts.maxBrowsers !== undefined) cliUse.maxBrowsers = opts.maxBrowsers

    const runOptions = {
      retries,
      outputDir,
      configUse,
      cliUse,
    }

    let results = []
    if (tasks.length > 0) {
      if (workers > 1 && tasks.length > 1) {
        results = await runWithWorkers(tasks, workers, runOptions, cwd, selected.length, listEnabled, listStream)
      } else {
        const pool = _internals.createBrowserPool(defaultPoolLaunchOptions(runOptions))
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
                printLiveResult(res, completed, selected.length, listEnabled, listStream)
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
      workers,
      shard: opts.shard ? opts.shard.raw : null,
      results,
    }

    if (listEnabled) {
      const failures = results.filter((r) => r.status === 'failed' || r.status === 'timedOut')
      if (failures.length > 0) {
        listStream.write('\nFailures:\n')
        failures.forEach((f, idx) => {
          listStream.write(`\n  ${idx + 1}) ${f.titlePath.join(' › ')}\n`)
          if (f.error) {
            listStream.write(`     ${f.error.stack || f.error.message}\n`)
          }
          for (const att of f.attachments || []) {
            if (att.path) listStream.write(`     attachment (${att.name}): ${att.path}\n`)
          }
        })
      }
      const parts = [
        `${passedCount} passed`,
        flakyCount ? `${flakyCount} flaky` : null,
        failedCount ? `${failedCount} failed` : null,
        skippedCount ? `${skippedCount} skipped` : null,
      ].filter(Boolean)
      listStream.write(`\n${parts.join(', ')} (${formatDuration(durationMs)})\n`)
    }

    if (reporters.includes('html')) {
      const reportPath = reportDir.endsWith('.html')
        ? path.resolve(cwd, reportDir)
        : path.resolve(cwd, reportDir, 'index.html')
      const written = await writeHtmlReport(report, reportPath)
      if (listEnabled) {
        listStream.write(`HTML report: ${written}\n`)
      }
    }

    if (jsonEnabled) {
      process.stdout.write(`${JSON.stringify(report, null, 2)}\n`)
    }

    return failedCount > 0 ? 1 : 0
  } finally {
    if (opts.updateSnapshots) {
      if (hadUpdateEnv) process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS = prevUpdateEnv
      else delete process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
    }
  }
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
  compileGrepMatcher,
  discoverTestFiles,
  findConfigPath,
  loadConfig,
  parseArgs,
  runCli,
  selectTests,
}
