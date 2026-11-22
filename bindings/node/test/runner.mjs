import assert from 'node:assert/strict'
import { execFile } from 'node:child_process'
import { once } from 'node:events'
import { existsSync } from 'node:fs'
import { mkdir, mkdtemp, readdir, readFile, rm, writeFile } from 'node:fs/promises'
import http from 'node:http'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createRequire } from 'node:module'
import { promisify } from 'node:util'
import { createFluxwrightFixtures, expect as runnerExpect, _internals } from 'fluxwright/test'

const require = createRequire(import.meta.url)
const { runCli, compileGrepMatcher } = require('../cli_test.js')

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

async function runCliSubprocess(args, extraEnv = {}, cwd = nodeRoot) {
  try {
    const { stdout, stderr } = await execFileAsync(process.execPath, [cliPath, ...args], {
      cwd,
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

// Regression: toEqual is strict structural equality (no 1 == '1' or 0 == false loose coercion).
runnerExpect({ a: 1 }).not.toEqual({ a: '1' })
runnerExpect({ a: 0 }).not.toEqual({ a: false })
runnerExpect(1).not.toEqual('1')
runnerExpect(0).not.toEqual(false)
assert.throws(() => runnerExpect({ a: 1 }).toEqual({ a: '1' }))
assert.throws(() => runnerExpect({ a: 0 }).toEqual({ a: false }))

// Regression: toThrow clones/resets /g regex across repeated calls.
{
  const re = /boom/g
  const boomFn = () => {
    throw new Error('boom')
  }
  runnerExpect(boomFn).toThrow(re)
  runnerExpect(boomFn).toThrow(re)
  runnerExpect(boomFn).toThrow(re)
}

// Regression: requestedFixtures does not strip `//` inside string/template literals.
assert.deepEqual([..._internals.requestedFixtures(async ({ page, context }) => [page, context])].sort(), ['context', 'page'])
assert.deepEqual(
  [..._internals.requestedFixtures(async ({ baseURL = 'http://localhost:3000', page }) => [baseURL, page])].sort(),
  ['baseURL', 'page'],
)
assert.deepEqual(
  [..._internals.requestedFixtures(async ({ baseURL = `https://example.com/a,b)`, context }) => [baseURL, context])].sort(),
  ['baseURL', 'context'],
)
assert.deepEqual([..._internals.requestedFixtures(async () => {})], [])

// Regression: sanitizePathSegment avoids collisions, strips trailing dots/spaces, neutralizes ./.. & Windows reserved names.
{
  const s1 = _internals.sanitizePathSegment('a/b')
  const s2 = _internals.sanitizePathSegment('a:b')
  assert.notEqual(s1, s2, 'distinct sanitized names a/b vs a:b must not collide')
  assert.ok(!_internals.sanitizePathSegment('trailing...').endsWith('.'))
  assert.ok(!_internals.sanitizePathSegment('trailing   ').endsWith(' '))
  assert.notEqual(_internals.sanitizePathSegment('.'), '.')
  assert.notEqual(_internals.sanitizePathSegment('..'), '..')
  assert.ok(!/^CON$/i.test(_internals.sanitizePathSegment('CON')))
  assert.ok(!/^NUL(\..*)?$/i.test(_internals.sanitizePathSegment('nul.txt')))
  assert.ok(!/^COM1$/i.test(_internals.sanitizePathSegment('COM1')))
  assert.ok(!/^LPT9$/i.test(_internals.sanitizePathSegment('LPT9')))
}

// Regression: compileGrepMatcher compiles real RegExp and throws clean error on invalid regex.
{
  const r1 = compileGrepMatcher('item-\\d+')
  assert.equal(r1.test('item-42'), true)
  assert.equal(r1.test('item-ab'), false)
  const r2 = compileGrepMatcher('(alpha|beta)-[a-z]+')
  assert.equal(r2.test('beta-xyz'), true)
  assert.equal(r2.test('gamma-xyz'), false)
  const r3 = compileGrepMatcher('/CASE/i')
  assert.equal(r3.test('mixed case here'), true)
  assert.throws(() => compileGrepMatcher('[unclosed'), /Invalid --grep regex/)
}

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

  // 6. Regression: fluxwright.config.mjs auto-discovery, test.use({ trace, video, recordVideo }),
  //    explicit CLI override precedence, and combined --reporter=list,json valid stdout JSON.
  {
    const cfgDir = join(tmpBase, 'cfg-project')
    const cfgTestsDir = join(cfgDir, 'e2e')
    const customRecDir = join(tmpBase, 'custom-rec')
    await mkdir(cfgTestsDir, { recursive: true })
    await writeFile(
      join(cfgDir, 'fluxwright.config.mjs'),
      `
import { defineConfig } from 'fluxwright/test'
export default defineConfig({
  testDir: './e2e',
  retries: 1,
  outputDir: './cfg-results',
  reportDir: './cfg-report',
  reporter: ['json'],
  use: {
    baseURL: ${JSON.stringify(base)},
    screenshot: 'off',
    trace: 'on',
    video: 'on',
  },
})
`,
      'utf8',
    )
    await writeFile(
      join(cfgTestsDir, 'cfg.test.mjs'),
      `
import { test, expect } from 'fluxwright/test'

test('config use honored and multi-page early close video retained', async ({ context, page }, info) => {
  await page.goto('/page-a')
  await expect(page).toHaveTitle('Page A')
  // Secondary page opened and closed before test finishes must still have its video retained!
  const p2 = await context.newPage()
  await p2.goto('/page-b')
  await expect(p2).toHaveTitle('Page B')
  await p2.close()
  await info.attach('debug-note', { body: 'persisted-body-payload', contentType: 'text/plain' })
})
`,
      'utf8',
    )
    const cfgRun = await runCliSubprocess([], {}, cfgDir)
    assert.equal(cfgRun.code, 0, `config run failed:\n${cfgRun.stdout}\n${cfgRun.stderr}`)
    const cfgJson = JSON.parse(cfgRun.stdout)
    assert.equal(cfgJson.results.length, 1)
    assert.equal(cfgJson.results[0].status, 'passed')
    const cfgOut = join(cfgDir, 'cfg-results')
    assert.equal((await findFiles(cfgOut, '.zip')).length, 1, 'config.use.trace=on should produce trace.zip')
    const cfgVideos = [...(await findFiles(cfgOut, '.avi')), ...(await findFiles(cfgOut, '.webm'))]
    assert.equal(cfgVideos.length, 2, `expected 2 videos (main + early-closed secondary page), got ${cfgVideos.length}`)
    const noteAtt = cfgJson.results[0].attachments.find((a) => a.name === 'debug-note')
    assert.ok(noteAtt?.path, 'non-image testInfo.attach body must be persisted to disk')
    assert.equal(await readFile(noteAtt.path, 'utf8'), 'persisted-body-payload')

    // Combined list,json must keep stdout as pure valid JSON and send list output to stderr.
    const comboRun = await runCliSubprocess(['--reporter=list,json'], {}, cfgDir)
    assert.equal(comboRun.code, 0, comboRun.stderr)
    const comboJson = JSON.parse(comboRun.stdout)
    assert.equal(comboJson.results.length, 1)
    assert.match(comboRun.stderr, /1 passed/)

    // Suite test.use({ trace: 'on', video: 'on' }) must NOT be clobbered when CLI omits --trace/--video;
    // explicit CLI --trace=off MUST override suite test.use({ trace: 'on' }).
    const useDir = join(tmpBase, 'use-precedence')
    const useOut1 = join(tmpBase, 'use-out-1')
    const useOut2 = join(tmpBase, 'use-out-2')
    await mkdir(useDir, { recursive: true })
    await writeFile(
      join(useDir, 'use.test.mjs'),
      `
import { test, expect } from 'fluxwright/test'
test.use({ baseURL: ${JSON.stringify(base)}, trace: 'on', video: 'on' })
test('suite use preserved', async ({ page }) => {
  await page.goto('/page-a')
  await expect(page).toHaveTitle('Page A')
})
`,
      'utf8',
    )
    const useRun1 = await runCliSubprocess([useDir, `--output=${useOut1}`, '--reporter=json'])
    assert.equal(useRun1.code, 0, useRun1.stderr)
    assert.equal((await findFiles(useOut1, '.zip')).length, 1, 'suite test.use({ trace: "on" }) must not be clobbered')
    const useVids1 = [...(await findFiles(useOut1, '.avi')), ...(await findFiles(useOut1, '.webm'))]
    assert.equal(useVids1.length, 1, 'suite test.use({ video: "on" }) must not be clobbered')

    const useRun2 = await runCliSubprocess([useDir, `--output=${useOut2}`, '--trace=off', '--reporter=json'])
    assert.equal(useRun2.code, 0, useRun2.stderr)
    assert.equal((await findFiles(useOut2, '.zip')).length, 0, 'explicit --trace=off must override suite test.use')
    const useVids2 = [...(await findFiles(useOut2, '.avi')), ...(await findFiles(useOut2, '.webm'))]
    assert.equal(useVids2.length, 1, 'unset --video must keep suite test.use({ video: "on" })')

    // test.use({ recordVideo }) forwarded directly without runner deleting it when video='off'.
    const recDir = join(tmpBase, 'rec-use')
    await mkdir(recDir, { recursive: true })
    await writeFile(
      join(recDir, 'rec.test.mjs'),
      `
import { test, expect } from 'fluxwright/test'
test.use({ baseURL: ${JSON.stringify(base)}, recordVideo: { dir: ${JSON.stringify(customRecDir)} } })
test('custom recordVideo forwarded', async ({ page }) => {
  await page.goto('/page-a')
  await expect(page).toHaveTitle('Page A')
})
`,
      'utf8',
    )
    const recRun = await runCliSubprocess([recDir, `--output=${join(tmpBase, 'rec-out')}`, '--reporter=json'])
    assert.equal(recRun.code, 0, recRun.stderr)
    const customVids = [...(await findFiles(customRecDir, '.avi')), ...(await findFiles(customRecDir, '.webm'))]
    assert.equal(customVids.length, 1, 'test.use({ recordVideo }) must forward to context and not delete video')
  }

  // 7. Regression: .spec.ts loading + repeated in-process runCli() cache busting + env restoration.
  {
    const tsDir = join(tmpBase, 'ts-tests')
    await mkdir(tsDir, { recursive: true })
    await writeFile(
      join(tsDir, 'typed.spec.ts'),
      `
import { test, expect } from 'fluxwright/test'

test('ts spec test 1', async () => {
  const value: number = 21 * 2
  expect(value).toBe(42)
})
`,
      'utf8',
    )
    const tsRun = await runCliSubprocess([tsDir, '--reporter=json'])
    assert.equal(tsRun.code, 0, `TS spec failed:\n${tsRun.stdout}\n${tsRun.stderr}`)
    const tsJson = JSON.parse(tsRun.stdout)
    assert.equal(tsJson.results.length, 1)
    assert.equal(tsJson.results[0].status, 'passed')

    // Repeated in-process runCli() on the same file must re-register tests every time AND restore env!
    const repeatDir = join(tmpBase, 'repeat-inproc')
    const repeatCounter = join(tmpBase, 'repeat-hits.txt')
    await mkdir(repeatDir, { recursive: true })
    await writeFile(repeatCounter, '0', 'utf8')
    await writeFile(
      join(repeatDir, 'rep.test.mjs'),
      `
import { readFileSync, writeFileSync } from 'node:fs'
import { test, expect } from 'fluxwright/test'
test('repeated test', async () => {
  const n = Number(readFileSync(${JSON.stringify(repeatCounter)}, 'utf8')) + 1
  writeFileSync(${JSON.stringify(repeatCounter)}, String(n), 'utf8')
  expect(n).toBeGreaterThan(0)
})
`,
      'utf8',
    )
    const prevSnap = process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
    delete process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
    try {
      const code1 = await runCli([repeatDir, '--update-snapshots', '--reporter=html', `--report-dir=${join(tmpBase, 'rep-h1')}`], nodeRoot)
      assert.equal(code1, 0)
      assert.equal(process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS, undefined, 'FLUXWRIGHT_UPDATE_SNAPSHOTS must be restored in finally')
      const code2 = await runCli([repeatDir, '--reporter=html', `--report-dir=${join(tmpBase, 'rep-h2')}`], nodeRoot)
      assert.equal(code2, 0)
      assert.equal(Number(await readFile(repeatCounter, 'utf8')), 2, 'repeated runCli() must re-execute cached ESM test file')
    } finally {
      if (prevSnap !== undefined) process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS = prevSnap
      else delete process.env.FLUXWRIGHT_UPDATE_SNAPSHOTS
    }
  }

  // 8. Regression: test.afterAll() runs without beforeAll AND surfaces errors as failures.
  {
    const hookDir = join(tmpBase, 'after-all')
    const hookMarker = join(tmpBase, 'after-all-ran.txt')
    await mkdir(hookDir, { recursive: true })
    await writeFile(
      join(hookDir, 'after_only.test.mjs'),
      `
import { writeFileSync } from 'node:fs'
import { test, expect } from 'fluxwright/test'

test.describe('suite without beforeAll', () => {
  test.afterAll(async () => {
    writeFileSync(${JSON.stringify(hookMarker)}, 'RAN', 'utf8')
  })
  test('passes', async () => {
    expect(1).toBe(1)
  })
})
`,
      'utf8',
    )
    const hookOkRun = await runCliSubprocess([join(hookDir, 'after_only.test.mjs'), '--reporter=json'])
    assert.equal(hookOkRun.code, 0, hookOkRun.stderr)
    assert.equal(await readFile(hookMarker, 'utf8'), 'RAN', 'afterAll must execute even when suite has no beforeAll')

    await writeFile(
      join(hookDir, 'after_throws.test.mjs'),
      `
import { test, expect } from 'fluxwright/test'

test.describe('suite with failing afterAll', () => {
  test.afterAll(async () => {
    throw new Error('afterAll teardown exploded')
  })
  test('body passes', async () => {
    expect(true).toBeTruthy()
  })
})
`,
      'utf8',
    )
    const hookFailRun = await runCliSubprocess([join(hookDir, 'after_throws.test.mjs'), '--reporter=json'])
    assert.equal(hookFailRun.code, 1, 'failing afterAll must cause CLI to exit 1')
    const hookFailJson = JSON.parse(hookFailRun.stdout)
    assert.ok(
      hookFailJson.results.some((r) => r.status === 'failed' && /afterAll teardown exploded/.test(r.error?.message || '')),
      'afterAll error must be surfaced as a failed result',
    )
  }

  // 9. Regression: flaky test preserves prior failure error & attachments, plus expect.soft in runner, plus --grep regex.
  {
    const miscDir = join(tmpBase, 'misc-reg')
    const miscOut = join(tmpBase, 'misc-out')
    const flakyCount2 = join(tmpBase, 'flaky2.txt')
    await mkdir(miscDir, { recursive: true })
    await writeFile(flakyCount2, '0', 'utf8')
    await writeFile(
      join(miscDir, 'misc.test.mjs'),
      `
import { readFileSync, writeFileSync } from 'node:fs'
import { test, expect } from 'fluxwright/test'

test('flaky-item-42', async ({ page }) => {
  const c = Number(readFileSync(${JSON.stringify(flakyCount2)}, 'utf8')) + 1
  writeFileSync(${JSON.stringify(flakyCount2)}, String(c), 'utf8')
  await page.goto(${JSON.stringify(base)} + '/page-a')
  if (c === 1) throw new Error('attempt-0-failure-reason')
  await expect(page).toHaveTitle('Page A')
})

test('soft-item-99', async ({ page }) => {
  await page.goto(${JSON.stringify(base)} + '/page-a')
  expect.soft(1).toBe(2)
  await expect.soft(page.locator('#msg')).toHaveText('WrongSoft', { timeout: 120 })
  // Must still execute after soft failures:
  await expect(page).toHaveTitle('Page A')
})
`,
      'utf8',
    )

    // --grep with real regex (\d+) selects flaky-item-42 only.
    const flakyRun = await runCliSubprocess([miscDir, '--grep=flaky-item-\\d+', '--retries=1', `--output=${miscOut}`, '--reporter=json'])
    assert.equal(flakyRun.code, 0, flakyRun.stderr)
    const flakyJson = JSON.parse(flakyRun.stdout)
    assert.equal(flakyJson.results.length, 1)
    assert.equal(flakyJson.results[0].status, 'flaky')
    assert.match(flakyJson.results[0].error?.message || '', /attempt-0-failure-reason/, 'flaky result must preserve prior failure error')
    assert.ok(
      flakyJson.results[0].attachments.some((a) => a.name === 'screenshot' && a.path),
      'flaky result must preserve prior failure screenshot attachment',
    )

    // Invalid --grep regex exits 1 cleanly.
    const badGrepRun = await runCliSubprocess([miscDir, '--grep=[unclosed'])
    assert.equal(badGrepRun.code, 1)
    assert.match(badGrepRun.stderr, /Invalid --grep regex/)

    // expect.soft collects both value and locator failures and fails test at end (with failure screenshot).
    const softRun = await runCliSubprocess([miscDir, '--grep=(soft)-item-[0-9]+', `--output=${miscOut}`, '--reporter=json'])
    assert.equal(softRun.code, 1)
    const softJson = JSON.parse(softRun.stdout)
    assert.equal(softJson.results.length, 1)
    assert.equal(softJson.results[0].status, 'failed')
    assert.match(softJson.results[0].error?.message || '', /2 soft assertion failures/)
    assert.match(softJson.results[0].error?.message || '', /WrongSoft/)
    assert.ok(
      softJson.results[0].attachments.some((a) => a.name === 'screenshot' && a.path),
      'soft assertion failure must trigger only-on-failure screenshot capture',
    )
  }

  // 10. Regression: worker SIGKILL exits non-zero (never 0!) and terminates sibling workers cleanly.
  {
    const killDir = join(tmpBase, 'sigkill')
    await mkdir(killDir, { recursive: true })
    await writeFile(
      join(killDir, 'w1.test.mjs'),
      `
import { test } from 'fluxwright/test'
test('kills worker', async () => {
  process.kill(process.pid, 'SIGKILL')
})
`,
      'utf8',
    )
    await writeFile(
      join(killDir, 'w2.test.mjs'),
      `
import { test, expect } from 'fluxwright/test'
test('sibling test', async () => {
  await new Promise((r) => setTimeout(r, 300))
  expect(1).toBe(1)
})
`,
      'utf8',
    )
    const killRun = await runCliSubprocess([killDir, '--workers=2', '--reporter=json'])
    assert.notEqual(killRun.code, 0, 'SIGKILL-terminated worker must NOT exit 0')
    assert.match(killRun.stderr, /Worker (terminated by signal SIGKILL|exited with code)/)
  }

  // 11. Regression: createFluxwrightFixtures wires trace and video in withPage() & vitest.context.
  {
    const fxTvDir = join(tmpBase, 'fx-trace-video')
    const fxTv = createFluxwrightFixtures({
      maxBrowsers: 1,
      baseURL: base,
      outputDir: fxTvDir,
      trace: 'on',
      video: 'on',
      screenshot: 'off',
    })
    try {
      await fxTv.withPage(async ({ page }) => {
        await page.goto('/page-a')
        await runnerExpect(page).toHaveTitle('Page A')
      }, { name: 'fx-tv-with-page' })

      await fxTv.vitest.browser({}, async (b) => {
        await fxTv.vitest.context({ browser: b }, async (ctx) => {
          await fxTv.vitest.page({ context: ctx }, async (p) => {
            await p.goto('/page-b')
            await runnerExpect(p).toHaveTitle('Page B')
          })
        }, { name: 'fx-tv-vitest' })
      })
    } finally {
      await fxTv.close()
    }
    const tvTraces = await findFiles(fxTvDir, '.zip')
    const tvVideos = [...(await findFiles(fxTvDir, '.avi')), ...(await findFiles(fxTvDir, '.webm'))]
    assert.equal(tvTraces.length, 2, `expected 2 traces from withPage + vitest, got ${tvTraces.length}`)
    assert.equal(tvVideos.length, 2, `expected 2 videos from withPage + vitest, got ${tvVideos.length}`)
  }
} finally {
  await rm(tmpBase, { recursive: true, force: true })
  server.close()
}

console.log('runner ok')
