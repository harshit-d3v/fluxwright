import { test, expect, mock } from 'claude-code/testing'

const ROOT = '/repo'

// A Fluxwright checkout at ROOT whose git knows the given tags.
function checkout(on, tags = '') {
  on('session.root', async () => ({ value: ROOT }))
  on('session.cwd', async () => ({ value: ROOT }))
  on('fs.exists', async ($, e) => ({ value: e.path === `${ROOT}/bindings/node/npm` }))
  on('process.run', async () => ({ value: { exitCode: 0, stdout: tags, stderr: '' } }))
}

// The shell beneath the plugin: records each command it is asked to run.
function shell(on, stdout = 'ok') {
  const ran = []
  on('tool.call', async ($, e) => {
    ran.push(e.command)
    return { result: { stdout, stderr: '', interrupted: false } }
  })
  return ran
}

async function bash($, command) {
  return $.tool.call({ tool: 'Bash', command })
}

test('npm publish is refused in a Fluxwright checkout', async ($, on) => {
  checkout(on)
  const ran = shell(on)
  for (const command of [
    'npm publish bindings/node --access public --ignore-scripts',
    'cd bindings/node && npx napi prepublish -t npm',
    'npm dist-tag add fluxwright@0.3.1 latest',
  ]) {
    expect((await bash($, command)).deny, command).toMatch(/does not publish/)
  }
  expect(ran).toEqual([])
})

test('outside a Fluxwright checkout the guards stand aside', async ($, on) => {
  on('session.root', async () => ({ value: '/elsewhere' }))
  on('fs.exists', async () => ({ value: false }))
  const ran = shell(on)
  await bash($, 'npm publish --access public')
  await bash($, 'cargo build')
  expect(ran).toEqual(['npm publish --access public', 'cargo build'])
})

test('release tags are refused, branches named like versions are not', async ($, on) => {
  checkout(on, 'v0.3.0\nv0.3.1\n')
  const ran = shell(on)
  for (const command of [
    'git push --tags',
    'git push origin main --follow-tags',
    'git push origin v0.3.1',
    'git push origin refs/tags/v0.3.2',
    'git tag v0.4.0 && git push origin v0.4.0',
    'gh release create v0.3.1 --generate-notes',
  ]) {
    expect((await bash($, command)).deny, command).toMatch(/release-npm\.yml/)
  }
  await bash($, 'git push -u origin v0.3.1-route-events')
  expect(ran).toEqual(['git push -u origin v0.3.1-route-events'])
})

test('timeout around a Chrome run is refused', async ($, on) => {
  checkout(on)
  const ran = shell(on)
  for (const command of [
    'timeout 120 node test/smoke.mjs',
    'timeout -k 5 600 /c/Users/harsh/.cargo/bin/cargo test -p fluxwright-core',
    'timeout 300 ./target/release/benchmarks --browsers 5',
  ]) {
    expect((await bash($, command)).deny, command).toMatch(/run_in_background/)
  }
  await bash($, 'timeout 5 curl -s http://127.0.0.1:3000/')
  await $.tool.call({ tool: 'PowerShell', command: 'timeout /t 5; node -v' })
  expect(ran).toEqual(['timeout 5 curl -s http://127.0.0.1:3000/', 'timeout /t 5; node -v'])
})

test('cargo falls back to ~/.cargo/bin when it is not on PATH', async ($, on) => {
  checkout(on)
  const ran = shell(on)
  await bash($, 'cargo test -p fluxwright-core')
  await $.tool.call({ tool: 'PowerShell', command: 'cargo build --release' })
  await bash($, '/c/Users/harsh/.cargo/bin/cargo build')
  expect(ran).toEqual([
    'export PATH="$PATH:$HOME/.cargo/bin"; cargo test -p fluxwright-core',
    "$env:PATH += [IO.Path]::PathSeparator + [IO.Path]::Combine($HOME, '.cargo', 'bin'); cargo build --release",
    '/c/Users/harsh/.cargo/bin/cargo build',
  ])
})

test('npm and GitHub tokens are masked in shell output', async ($, on) => {
  checkout(on)
  // Built at run time so the repo holds nothing shaped like a real token.
  const npm = 'npm_' + 'x'.repeat(36)
  const github = 'ghp_' + 'y'.repeat(36)
  shell(on, `//registry.npmjs.org/:_authToken=${npm}\nGH_TOKEN=${github}\n`)
  const r = await bash($, 'cat ~/.npmrc')
  expect(r.result.stdout).toBe('//registry.npmjs.org/:_authToken=npm_[redacted]\nGH_TOKEN=gh_[redacted]\n')
})

// A 0.3.2 release where only the win32 package reached npm.
function release(on, darwinBinary) {
  const manifests = {
    [`${ROOT}/bindings/node/package.json`]: { name: 'fluxwright', version: '0.3.2' },
    [`${ROOT}/bindings/node/npm/win32-x64-msvc/package.json`]: { name: 'fluxwright-win32-x64-msvc', version: '0.3.2', main: 'fluxwright.win32-x64-msvc.node' },
    [`${ROOT}/bindings/node/npm/darwin-arm64/package.json`]: { name: 'fluxwright-darwin-arm64', version: '0.3.2', main: 'fluxwright.darwin-arm64.node' },
  }
  on('session.root', async () => ({ value: ROOT }))
  on('fs.exists', async ($, e) => ({ value: e.path.endsWith('/npm') || (e.path.endsWith('.node') && (darwinBinary || !e.path.includes('darwin'))) }))
  on('fs.list', async () => ({ value: ['win32-x64-msvc', 'darwin-arm64'].map((name) => ({ name, kind: 'dir', size: 0, isLink: false })) }))
  on('fs.read', async ($, e) => ({ value: JSON.stringify(manifests[e.path]) }))
  mock.clock(on, { now: 1 })
  const asked = []
  on('http.fetch', async ($, e) => {
    asked.push(e.url)
    const ok = e.url.includes('win32')
    return { value: { status: ok ? 200 : 404, ok, headers: {}, text: '' } }
  })
  return asked
}

test('/release-check lists what is missing, platform packages first', async ($, on) => {
  const asked = release(on, true)
  const { text } = await $.command.run({ command: 'release-check', args: '' })
  expect(asked).toContain('https://registry.npmjs.org/fluxwright/0.3.2?t=1')
  expect(text).toMatch(/fluxwright-win32-x64-msvc\s+0\.3\.2\s+published/)
  expect(text).toMatch(/fluxwright-darwin-arm64\s+0\.3\.2\s+missing/)
  expect(text).toContain(
    `  npm publish ${ROOT}/bindings/node/npm/darwin-arm64 --access public\n` +
      `  npm publish ${ROOT}/bindings/node --access public --ignore-scripts`,
  )
})

test('/release-check holds fluxwright back while a platform binary is missing', async ($, on) => {
  release(on, false)
  const { text } = await $.command.run({ command: 'release-check', args: '' })
  expect(text).toContain('fluxwright-darwin-arm64 has no fluxwright.darwin-arm64.node yet')
  expect(text).not.toContain('npm publish')
})
