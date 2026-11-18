// fluxwright-dev: guardrails for working on Fluxwright with Claude Code.
//
// The guards and the cargo fix act only in a Fluxwright checkout (a project
// root with bindings/node/npm). Token redaction acts everywhere.

const PUBLISH = /\b(npm|pnpm|yarn)(\.cmd)?\s+(publish|unpublish|deprecate|dist-tag\s+(add|rm)|owner\s+(add|rm))\b|\bnapi(\.cmd)?\s+prepublish\b/
const RELEASE = /\bgh(\.exe)?\s+release\s+(create|delete|edit|upload)\b/
const PUSH = /\bgit(\.exe)?(\s+-C\s+\S+)?\s+push\b([^;&|\n]*)/g
const TAG = /\bgit(\.exe)?(\s+-C\s+\S+)?\s+tag\b([^;&|\n]*)/g
// ponytail: matches the usual launchers and target/ binaries; a Chrome run
// started any other way (a shell script, say) is not caught.
const TIMEOUT = /(^|[\s;&|(])timeout\s[^;&|\n]*?(\b(node|npm|npx|pnpm|cargo)(\.exe|\.cmd)?\b|\btarget[\/\\])/
const CARGO = /(^|[\s;&|(])cargo(\.exe)?\b/
const TOKENS: [RegExp, string][] = [
  [/npm_[A-Za-z0-9]{36}/g, 'npm_[redacted]'],
  [/gh[pousr]_[A-Za-z0-9]{36,}/g, 'gh_[redacted]'],
  [/github_pat_[A-Za-z0-9_]{22,}/g, 'github_pat_[redacted]'],
]

const NO_PUBLISH =
  'fluxwright-dev: Claude does not publish Fluxwright to npm. Give the user the commands to run themselves: ' +
  'each platform package first (npm publish bindings/node/npm/<platform> --access public), then ' +
  'npm publish bindings/node --access public --ignore-scripts. /release-check lists what is missing.'
const NO_TAGS =
  'fluxwright-dev: pushing a v* tag starts release-npm.yml, which publishes to npm, and gh release edits the ' +
  'public release page. Claude does neither in this repo: give the user the command to run themselves.'
const NO_TIMEOUT =
  'fluxwright-dev: do not wrap Chrome runs in timeout. Killing the run orphans Chrome, which keeps the output ' +
  'pipe open and hangs the shell. Run the command with run_in_background instead and read its output when it ends.'

export function register(on) {
  on('session.start', async ($, e, next) => {
    if (await inFluxwright($)) {
      await $.command.register({
        name: 'release-check',
        description: 'Show which Fluxwright npm packages are missing from the registry, with the commands to publish them',
      })
    }
    return next(e)
  })

  on('command.run', { command: 'release-check' }, async ($) => ({ text: await releaseCheck($) }))

  on('tool.call', { tool: ['Bash', 'PowerShell'] }, async ($, e, next) => {
    const verdict = await check($, e.tool, e.command ?? '')
    if (verdict.deny) return { deny: verdict.deny }
    const r = await next(verdict.command ? { ...e, command: verdict.command } : e)
    return redact(r)
  })
}

async function inFluxwright($) {
  return $.fs.exists((await $.session.root()) + '/bindings/node/npm')
}

async function check($, tool, command) {
  const publish = PUBLISH.test(command)
  const release = RELEASE.test(command)
  const push = command.search(PUSH) >= 0
  const timeout = tool === 'Bash' && TIMEOUT.test(command)
  const cargo = CARGO.test(command) && !/\.cargo[\/\\]bin/.test(command)
  if (!(publish || release || push || timeout || cargo)) return {}
  if (!(await inFluxwright($))) return {}
  if (publish) return { deny: NO_PUBLISH }
  if (release || (push && (await pushesTag($, command)))) return { deny: NO_TAGS }
  if (timeout) return { deny: NO_TIMEOUT }
  if (!cargo) return {}
  // Appending keeps any cargo already on PATH; ~/.cargo/bin is only the fallback.
  return {
    command:
      tool === 'Bash'
        ? `export PATH="$PATH:$HOME/.cargo/bin"; ${command}`
        : `$env:PATH += [IO.Path]::PathSeparator + [IO.Path]::Combine($HOME, '.cargo', 'bin'); ${command}`,
  }
}

// A push sends a tag when it says --tags, --follow-tags, --mirror or
// refs/tags/, or when a ref it names is a local tag (or one this same command
// creates with git tag). Branches such as v0.3.1-route-events still push.
async function pushesTag($, command) {
  const tags = new Set()
  for (const m of command.matchAll(TAG)) for (const word of m[3].trim().split(/\s+/)) tags.add(word)
  let listed = false
  for (const m of command.matchAll(PUSH)) {
    const args = m[3].trim().split(/\s+/).filter(Boolean)
    if (args.some((a) => ['--tags', '--follow-tags', '--mirror'].includes(a) || a.includes('refs/tags/'))) return true
    const refs = args.filter((a) => !a.startsWith('-')).slice(1)
    if (refs.length === 0) continue
    if (!listed) {
      listed = true
      try {
        const r = await $.process.run(['git', 'tag', '--list'], { cwd: await $.session.cwd() })
        for (const line of r.stdout.split(/\r?\n/)) tags.add(line.trim())
      } catch {
        // No git on PATH: judge by the refs alone.
      }
    }
    if (refs.some((ref) => ref.replace(/^\+/, '').split(':').some((part) => tags.has(part)))) return true
  }
  return false
}

function hide(text) {
  return typeof text === 'string' ? TOKENS.reduce((s, [re, mask]) => s.replace(re, mask), text) : text
}

function redact(r) {
  if (r.deny !== undefined) return r
  if (r.isError) {
    // Hooks cannot build an error result, so a refusal carries the masked
    // text: the model reads it as an error, as it would have.
    const text = r.text ?? (typeof r.result === 'string' ? r.result : '')
    return hide(text) === text ? r : { deny: hide(text) }
  }
  const out = r.result ?? {}
  const stdout = hide(out.stdout)
  const stderr = hide(out.stderr)
  if (stdout === out.stdout && stderr === out.stderr) return r
  return { result: { ...out, stdout, stderr } }
}

async function releaseCheck($) {
  const root = await $.session.root()
  const sep = root.includes('\\') ? '\\' : '/'
  const node = [root, 'bindings', 'node'].join(sep)
  if (!(await $.fs.exists(node + sep + 'npm'))) return `No bindings${sep}node${sep}npm under ${root}: not a Fluxwright checkout.`

  const platforms = (await $.fs.list(node + sep + 'npm')).filter((d) => d.kind === 'dir').map((d) => node + sep + 'npm' + sep + d.name)
  const now = await $.clock.now()
  const rows = await Promise.all(
    [...platforms, node].map(async (dir) => {
      const pkg = JSON.parse(await $.fs.read(dir + sep + 'package.json'))
      let state
      try {
        const res = await $.http.fetch(`https://registry.npmjs.org/${pkg.name}/${pkg.version}?t=${now}`)
        state = res.status === 200 ? 'published' : res.status === 404 ? 'missing' : `registry answered ${res.status}`
      } catch (err) {
        state = `registry unreachable (${err instanceof Error ? err.message : String(err)})`
      }
      const binary = dir === node || (await $.fs.exists(dir + sep + pkg.main))
      return { dir, name: pkg.name, version: pkg.version, state, binary, main: pkg.main }
    }),
  )

  const width = Math.max(...rows.map((r) => r.name.length))
  const lines = rows.map((r) => `  ${r.name.padEnd(width)}  ${r.version}  ${r.state}`)
  if (new Set(rows.map((r) => r.version)).size > 1) lines.push('', 'The versions differ: bump them all to one version before publishing.')

  const missing = rows.filter((r) => r.state === 'missing')
  if (missing.length === 0) return [`npm against bindings${sep}node:`, ...lines].join('\n')

  const commands = []
  const notes = []
  for (const r of missing) {
    // The main package goes last, and only once every platform package can go.
    if (r.dir === node) {
      if (notes.length === 0) commands.push(`npm publish ${r.dir} --access public --ignore-scripts`)
    } else if (r.binary) commands.push(`npm publish ${r.dir} --access public`)
    else notes.push(`${r.name} has no ${r.main} yet: download the release-npm.yml artifacts and run npm run artifacts first.`)
  }
  return [
    `npm against bindings${sep}node:`,
    ...lines,
    '',
    ...(notes.length ? [...notes, ''] : []),
    ...(commands.length ? ['Publish these yourself, in this order (platform packages first):', ...commands.map((c) => '  ' + c)] : []),
  ].join('\n')
}
