import assert from 'node:assert/strict'
import { execFile } from 'node:child_process'
import { once } from 'node:events'
import { existsSync } from 'node:fs'
import { mkdir, mkdtemp, readdir, readFile, rm, writeFile } from 'node:fs/promises'
import http from 'node:http'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { promisify } from 'node:util'
import { createFluxwrightFixtures, expect as runnerExpect, _internals } from 'fluxwright/test'

const execFileAsync = promisify(execFile)
const __dirname = dirname(fileURLToPath(import.meta.url))
const nodeRoot = resolve(__dirname, '..')
const cliPath = join(nodeRoot, 'cli_test.js')

const server = http.createServer((req, res) => {
  res.setHeader('Content-Type', 'text/html')
  if (req.url === '/page-a') {
    return res.end('<title>Page A</title><div id="msg">Alpha</div>')
  }
  if (req.url === '/page-b') {
    return res.end('<title>Page B</title><div id="msg">Beta</div>')
  }
  return res.end('<title>Runner Home</title><h1 id="hdr">Welcome</h1>')
})
server.listen(0, '127.0.0.1')
await once(server, 'listening')
server.unref()
const base = `http://127.0.0.1:${server.address().port}`

async function runCliSubprocess(args, extraEnv = {}) {
  try {
    const { stdout, stderr } = await execFileAsync(process.execPath, [cliPath, ...args], {
      cwd: nodeRoot,
      env: { ...process.env, ...extraEnv },
      timeout: 120_000,
    })
    return { code: 0, stdout, stderr }
  } catch (err) {
    return {
      code: typeof err.code === 'number' ? err.code : 1,
      stdout: err.stdout || '',
      stderr: err.stderr || '',
    }
  }
}

async function findFiles(dir, ext) {
  const out = []
  if (!existsSync(dir)) return out
  const entries = await readdir(dir, { withFileTypes: true })
  for (const e of entries) {
    const full = join(dir, e.name)
    if (e.isDirectory()) out.push(...(await findFiles(full, ext)))
    else if (e.name.endsWith(ext)) out.push(full)
  }
  return out
}

// 1. Unit checks for value assertions & fixture parameter parser.
runnerExpect(42).toBe(42)
runnerExpect({ a: 1, b: [2, 3] }).toEqual({ a: 1, b: [2, 3] })
runnerExpect(['x', 'y']).toContain('y')
runnerExpect('fluxwright').toMatch(/^flux/)
runnerExpect(null).toBeNull()
runnerExpect(undefined).toBeUndefined()
runnerExpect('ok').not.toBe('no')
runnerExpect(() => {
  throw new Error('boom')
}).toThrow('boom')
assert.deepEqual([..._internals.requestedFixtures(async ({ page, context }) => [page, context])].sort(), ['context', 'page'])
assert.deepEqual([..._internals.requestedFixtures(async () => {})], [])

const tmpBase = await mkdtemp(join(__dirname, '.tmp-runner-'))
try {
  // 2. Test createFluxwrightFixtures() shared browser pool, isolation, Vitest/node:test helpers, & failure screenshot.
  const fxOutDir = join(tmpBase, 'fx-results')
  const fx = createFluxwrightFixtures({ maxBrowsers: 1, baseURL: base, outputDir: fxOutDir })
  let firstBrowser
  try {
    await fx.withPage(async ({ browser, context, page }) => {
      firstBrowser = browser
      assert.ok(context)
      await page.goto('/page-a')
      await runnerExpect(page).toHaveTitle('Page A')
      await runnerExpect(page.locator('#msg')).toHaveText('Alpha')
      await page.evaluate(() => localStorage.setItem('leaked', 'yes'))
    })

    await fx.withPage(async ({ browser, page }) => {
      assert.equal(browser, firstBrowser, 'createFluxwrightFixtures must reuse one pooled Browser')
      await page.goto('/page-b')
      assert.equal(await page.evaluate(() => localStorage.getItem('leaked')), null, 'contexts must be isolated')
      await runnerExpect(page.locator('#msg')).toHaveText('Beta')
    })

    assert.equal(fx.getLaunchCount(), 1)

    // Verify failure screenshot capture inside fx.withPage.
    await assert.rejects(
      () =>
        fx.withPage(async ({ page }) => {
          await page.goto('/page-a')
          throw new Error('fx boom')
        }, { name: 'fx-failure-shot' }),
      /fx boom/,
    )
    const fxShots = await findFiles(fxOutDir, '.png')
    assert.ok(fxShots.length >= 1, 'createFluxwrightFixtures should capture failure screenshot')
    const fxPng = await readFile(fxShots[0])
    assert.deepEqual(fxPng.subarray(0, 4), Buffer.from([0x89, 0x50, 0x4e, 0x47]))

    // Verify acquire({ name, after }) & vitest adapter shape.
    let afterCb = null
    const acquired = await fx.acquire({
      name: 'node-test-ctx',
      after: (fn) => {
        afterCb = fn
      },
    })
    assert.equal(acquired.browser, firstBrowser)
    assert.equal(typeof afterCb, 'function')
    await acquired.page.goto('/page-a')
    await afterCb()

    await fx.vitest.browser({}, async (b) => {
      assert.equal(b, firstBrowser)
      await fx.vitest.context({ browser: b }, async (ctx) => {
        await fx.vitest.page({ context: ctx }, async (p) => {
          await p.goto('/page-b')
          await runnerExpect(p).toHaveTitle('Page B')
        })
      })
    })
  } finally {
    await fx.close()
  }

  // 3. Test fluxwright-test CLI: shared browser, hooks, skip, retries, list + html report.
  const passingDir = join(tmpBase, 'passing')
  const passingOut = join(tmpBase, 'passing-out')
  const passingReport = join(tmpBase, 'passing-report')
  const flakyCounterFile = join(tmpBase, 'flaky-count.txt')
  await writeFile(flakyCounterFile, '0', 'utf8')

  await mkdir(passingDir, { recursive: true })

  await writeFile(
    join(passingDir, 'a.test.mjs'),
    `
import assert from 'node:assert/strict'
import { test, expect } from 'fluxwright/test'

test.use({ baseURL: ${JSON.stringify(base)} })
const seenBrowsers = new Set()
const hookOrder = []

test.beforeEach(async () => { hookOrder.push('before') })
test.afterEach(async () => { hookOrder.push('after') })

test('shared pool 1', async ({ page, browser }, info) => {
  seenBrowsers.add(browser)
  assert.ok(info.outputDir.length > 0)
  await page.goto('/page-a')
  await expect(page).toHaveTitle('Page A')
  await page.evaluate(() => localStorage.setItem('iso', 'test1'))
})

test('shared pool 2', async ({ page, browser }) => {
  seenBrowsers.add(browser)
  assert.equal(seenBrowsers.size, 1, 'tests in a worker must share one pooled Browser')
  await page.goto('/page-b')
  await expect(page.locator('#msg')).toHaveText('Beta')
  assert.equal(await page.evaluate(() => localStorage.getItem('iso')), null)
  assert.deepEqual(hookOrder, ['before', 'after', 'before'])
})

test.skip('skipped test', async () => {
  throw new Error('skipped test must not execute')
})
`,
    'utf8',
  )

  await writeFile(
    join(passingDir, 'b.test.mjs'),
    `
import { readFileSync, writeFileSync } from 'node:fs'
import { test, expect } from 'fluxwright/test'

test.describe('suite b', () => {
  test('passes on b', async ({ page }) => {
    await page.goto(${JSON.stringify(base)} + '/page-a')
    await expect(page.locator('#msg')).toHaveText('Alpha')
  })

  test('flaky once', async ({ page }) => {
    const file = ${JSON.stringify(flakyCounterFile)}
    const count = Number(readFileSync(file, 'utf8')) + 1
    writeFileSync(file, String(count), 'utf8')
    await page.goto(${JSON.stringify(base)} + '/page-b')
    if (count < 2) throw new Error('first attempt fails')
    await expect(page).toHaveTitle('Page B')
  })
})
`,
    'utf8',
  )

  const passRun = await runCliSubprocess([
    passingDir,
    '--workers=1',
    '--retries=1',
    '--reporter=list,html',
    `--output=${passingOut}`,
    `--report-dir=${passingReport}`,
  ])
  assert.equal(passRun.code, 0, `passing suite failed:\n${passRun.stdout}\n${passRun.stderr}`)
  assert.match(passRun.stdout, /shared pool 1/)
  assert.match(passRun.stdout, /flaky/)
  assert.match(passRun.stdout, /skipped/)

  const passHtml = await readFile(join(passingReport, 'index.html'), 'utf8')
  assert.ok(passHtml.startsWith('<!DOCTYPE html>'))
  assert.match(passHtml, /Fluxwright Test Report/)
  assert.match(passHtml, /shared pool 1/)
  assert.match(passHtml, /flaky once/)

  // 4. Test deterministic --shard=1/2 & --shard=2/2 and --workers=2.
  const shardDir = join(tmpBase, 'shards')
  await mkdir(shardDir, { recursive: true })
  await writeFile(
    join(shardDir, 's1.test.mjs'),
    `
import { test, expect } from 'fluxwright/test'
test('shard test 1', async ({ page }) => {
  await page.goto(${JSON.stringify(base)} + '/page-a')
  await expect(page).toHaveTitle('Page A')
})
test('shard test 2', async () => {
  expect(2 + 2).toBe(4)
})
`,
    'utf8',
  )
  await writeFile(
    join(shardDir, 's2.test.mjs'),
    `
import { test, expect } from 'fluxwright/test'
test('shard test 3', async ({ page }) => {
  await page.goto(${JSON.stringify(base)} + '/page-b')
  await expect(page).toHaveTitle('Page B')
})
test('shard test 4', async () => {
  expect('fluxwright').toContain('flux')
})
`,
    'utf8',
  )

  const shard1Run = await runCliSubprocess([shardDir, '--shard=1/2', '--reporter=json'])
  assert.equal(shard1Run.code, 0, shard1Run.stderr)
  const shard1Json = JSON.parse(shard1Run.stdout)
  assert.equal(shard1Json.shard, '1/2')
  assert.equal(shard1Json.results.length, 2)

  const shard2Run = await runCliSubprocess([shardDir, '--shard=2/2', '--reporter=json'])
  assert.equal(shard2Run.code, 0, shard2Run.stderr)
  const shard2Json = JSON.parse(shard2Run.stdout)
  assert.equal(shard2Json.shard, '2/2')
  assert.equal(shard2Json.results.length, 2)

  const titles1 = new Set(shard1Json.results.map((r) => r.title))
  const titles2 = new Set(shard2Json.results.map((r) => r.title))
  for (const t of titles1) assert.equal(titles2.has(t), false, `duplicate test across shards: ${t}`)
  assert.deepEqual([...titles1, ...titles2].sort(), ['shard test 1', 'shard test 2', 'shard test 3', 'shard test 4'])

  // Also test --workers=2 parallel execution across the 2 files.
  const parallelRun = await runCliSubprocess([shardDir, '--workers=2', '--reporter=json'])
  assert.equal(parallelRun.code, 0, parallelRun.stderr)
  const parallelJson = JSON.parse(parallelRun.stdout)
  assert.equal(parallelJson.results.length, 4)
  assert.ok(parallelJson.results.every((r) => r.status === 'passed'))

  // 5. Test failure screenshot capture, --grep, --update-snapshots, and HTML embedded failure screenshot.
  const failDir = join(tmpBase, 'fail')
  const failOut = join(tmpBase, 'fail-out')
  const failReport = join(tmpBase, 'fail-report')
  await mkdir(failDir, { recursive: true })
  await writeFile(
    join(failDir, 'fail.test.mjs'),
    `
import { test, expect } from 'fluxwright/test'

test('intentional failure', async ({ page }) => {
  await page.goto(${JSON.stringify(base)} + '/page-a')
  await expect(page.locator('#msg')).toHaveText('NeverMatches', { timeout: 150 })
})

test('passing neighbor', async ({ page }) => {
  await page.goto(${JSON.stringify(base)} + '/page-b')
  await expect(page).toHaveTitle('Page B')
})

test('update snapshots env check', async () => {
  expect(process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS).toBe('1')
})
`,
    'utf8',
  )

  const snapRun = await runCliSubprocess([failDir, '--grep=update snapshots', '--update-snapshots', '--reporter=json'])
  assert.equal(snapRun.code, 0, snapRun.stderr)
  const snapJson = JSON.parse(snapRun.stdout)
  assert.equal(snapJson.results.length, 1)
  assert.equal(snapJson.results[0].status, 'passed')

  const failRun = await runCliSubprocess([
    failDir,
    '--grep=intentional failure|passing neighbor',
    '--reporter=list,html',
    '--trace=retain-on-failure',
    '--video=retain-on-failure',
    `--output=${failOut}`,
    `--report-dir=${failReport}`,
  ])
  assert.equal(failRun.code, 1, 'failing test suite must exit 1')
  assert.match(failRun.stdout, /1 passed, 1 failed/)

  const failShots = await findFiles(failOut, '.png')
  assert.equal(failShots.length, 1, 'expected exactly one failure screenshot')
  const shotBytes = await readFile(failShots[0])
  assert.deepEqual(shotBytes.subarray(0, 4), Buffer.from([0x89, 0x50, 0x4e, 0x47]))

  const failTraces = await findFiles(failOut, '.zip')
  assert.equal(failTraces.length, 1, 'expected exactly one failure trace zip')
  const traceBytes = await readFile(failTraces[0])
  assert.deepEqual(traceBytes.subarray(0, 4), Buffer.from([0x50, 0x4b, 0x03, 0x04]))

  const failVideos = [...(await findFiles(failOut, '.avi')), ...(await findFiles(failOut, '.webm'))]
  assert.equal(failVideos.length, 1, 'expected exactly one failure video')
  const vidBytes = await readFile(failVideos[0])
  const vidMagic = vidBytes.subarray(0, 4)
  assert.ok(
    vidMagic.equals(Buffer.from('RIFF', 'ascii')) || vidMagic.equals(Buffer.from([0x1a, 0x45, 0xdf, 0xa3])),
    'failure video must have valid AVI or WebM header',
  )

  const failHtml = await readFile(join(failReport, 'index.html'), 'utf8')
  assert.ok(failHtml.startsWith('<!DOCTYPE html>'))
  assert.match(failHtml, /intentional failure/)
  assert.match(failHtml, /passing neighbor/)
  assert.match(failHtml, /toHaveText/)
  assert.match(failHtml, /trace\.zip/)
  assert.match(failHtml, /video\.(?:avi|webm)/)
  assert.match(failHtml, /data:image\/png;base64,[A-Za-z0-9+/=]+/)
} finally {
  await rm(tmpBase, { recursive: true, force: true })
  server.close()
}

console.log('runner ok')
