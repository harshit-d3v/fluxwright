import assert from 'node:assert/strict'
import { once } from 'node:events'
import http from 'node:http'
import fluxwright, { chromium, expect } from '../addon.js'

assert.equal(fluxwright.expect, expect)

const server = http.createServer((req, res) => {
  res.setHeader('Content-Type', 'text/html')
  if (req.url === '/done') {
    return res.end('<title>Done Title</title><p id="done">Finished</p>')
  }
  res.end(`<!doctype html>
    <title>Start Title</title>
    <div id="vis" style="display:none">Hidden</div>
    <div id="hide">Visible</div>
    <button id="btn" disabled>Go</button>
    <input id="chk" type="checkbox">
    <input id="edt" readonly value="before">
    <p id="txt">  Loading   data  </p>
    <div id="cls" class="pill idle">Idle</div>
    <ul id="items"><li>One</li></ul>
    <div class="dup">First</div><div class="dup">Second</div>
    <div id="swap-host"><span id="swap">NotAnInput</span></div>
  `)
})
server.listen(0, '127.0.0.1')
await once(server, 'listening')
server.unref()
const base = `http://127.0.0.1:${server.address().port}`

// Option & argument validation before/without hitting browser.
assert.throws(() => expect(null), TypeError)
assert.throws(() => expect({}), TypeError)
assert.throws(() => expect.configure({ timeout: -1 }), TypeError)
assert.throws(() => expect.configure({ bogus: true }), TypeError)

const browser = await chromium.launch({ maxBrowsers: 1 })
try {
  const page = await browser.newPage({ baseURL: base })
  page.setDefaultTimeout(10_000)
  await page.goto('/start')

  // Immediate assertions prior to delayed mutation.
  await expect(page.locator('#vis')).toBeHidden()
  await expect(page.locator('#btn')).toBeDisabled()
  await expect(page.locator('#chk')).not.toBeChecked()
  await expect(page.locator('#chk')).toBeChecked({ checked: false })
  await expect(page.locator('#edt')).not.toBeEditable()
  await expect(page.locator('#late')).not.toBeAttached()
  await expect(page).not.toHaveURL('/done')

  // Schedule delayed async DOM mutations after immediate checks.
  await page.evaluate(() => {
    setTimeout(() => {
      document.title = 'Updated   Title'
      history.pushState({}, '', '/done')
      document.getElementById('vis').style.display = 'block'
      document.getElementById('hide').style.display = 'none'
      const btn = document.getElementById('btn')
      btn.disabled = false
      btn.setAttribute('data-status', 'ready-42')
      document.getElementById('chk').checked = true
      const edt = document.getElementById('edt')
      edt.readOnly = false
      edt.value = 'after'
      document.getElementById('txt').textContent = '  Loaded\n  42   items  '
      document.getElementById('cls').className = 'pill active primary'
      const li = document.createElement('li')
      li.textContent = 'Two'
      document.getElementById('items').appendChild(li)
      const late = document.createElement('div')
      late.id = 'late'
      late.style.display = 'none'
      document.body.appendChild(late)
      const dups = document.querySelectorAll('.dup')
      if (dups.length > 1) dups[1].remove()
      const inp = document.createElement('input')
      inp.id = 'swap'
      inp.value = 'swapped'
      document.getElementById('swap-host').replaceChildren(inp)
    }, 120)
  })

  // Delayed async assertions.
  await expect(page).toHaveTitle('Updated Title')
  await expect(page).toHaveTitle(/Updated\s+Title/g)
  await expect(page).not.toHaveTitle('Start Title')
  await expect(page).toHaveURL('/done')
  await expect(page).toHaveURL(`${base}/done`)
  await expect(page).toHaveURL(/\/done$/g)

  await expect(page.locator('#vis')).toBeVisible()
  await expect(page.locator('#vis')).not.not.toBeVisible()
  await expect(page.locator('#hide')).toBeHidden()
  await expect(page.locator('#hide')).not.toBeVisible()
  await expect(page.locator('#late')).toBeAttached()
  await expect(page.locator('#late')).toBeHidden()

  await expect(page.locator('#btn')).toBeEnabled()
  await expect(page.locator('#btn')).not.toBeDisabled()
  await expect(page.locator('#btn')).toHaveAttribute('data-status', 'ready-42')
  await expect(page.locator('#btn')).toHaveAttribute('data-status', /^ready-\d+$/)
  await expect(page.locator('#btn')).not.toHaveAttribute('data-status', 'pending')
  await expect(page.locator('#btn')).not.toHaveAttribute('data-missing', 'any')

  await expect(page.locator('#chk')).toBeChecked()
  await expect(page.locator('#edt')).toBeEditable()
  await expect(page.locator('#edt')).toHaveValue('after')
  await expect(page.locator('#edt')).toHaveValue(/^aft/)
  await expect(page.locator('#edt')).not.toHaveValue('before')

  await expect(page.locator('#txt')).toHaveText('Loaded 42 items')
  await expect(page.locator('#txt')).toHaveText(/Loaded \d+ items/g)
  await expect(page.locator('#txt')).toContainText('42 items')
  await expect(page.locator('#txt')).toContainText(/\d+ items/)
  await expect(page.locator('#txt')).not.toHaveText('Loading data')
  await expect(page.locator('#txt')).not.toContainText('Loading')

  await expect(page.locator('#cls')).toHaveClass('pill active primary')
  await expect(page.locator('#cls')).toHaveClass(/\bactive\b/)
  await expect(page.locator('#cls')).not.toHaveClass('pill idle')

  await expect(page.locator('#items li')).toHaveCount(2)
  await expect(page.locator('#items li')).not.toHaveCount(1)
  await expect(page.locator('#never')).toHaveCount(0)

  // Transient locator errors (strict violation & wrong element tag) are ignored until resolved.
  await expect(page.locator('.dup')).toHaveText('First')
  await expect(page.locator('#swap')).toHaveValue('swapped')

  // Positive toBeDisabled after delayed toggle.
  await page.evaluate(() => setTimeout(() => { document.getElementById('btn').disabled = true }, 100))
  await expect(page.locator('#btn')).toBeDisabled()

  // Validation errors on matcher arguments/options.
  await assert.rejects(() => expect(page.locator('#txt')).toHaveText(123), TypeError)
  await assert.rejects(() => expect(page.locator('#items li')).toHaveCount(-1), TypeError)
  await assert.rejects(() => expect(page.locator('#txt')).toHaveText('x', { bogus: 1 }), TypeError)
  await assert.rejects(() => expect(page.locator('#txt')).toHaveText('x', { timeout: NaN }), TypeError)

  // expect.configure({ timeout }) + per-call override + diagnostics.
  const shortExpect = expect.configure({ timeout: 200 })
  const inheritedExpect = shortExpect.configure({})
  const started = Date.now()
  await assert.rejects(
    () => inheritedExpect(page.locator('#txt')).toHaveText('Never'),
    (err) => {
      assert.match(err.message, /toHaveText/)
      assert.match(err.message, /200ms/)
      assert.match(err.message, /Expected: "Never"/)
      assert.match(err.message, /Received: "Loaded 42 items"/)
      return true
    },
  )
  assert.ok(Date.now() - started < 3000, `configured timeout took too long: ${Date.now() - started}ms`)

  // .not on missing element must fail rather than falsely pass.
  await assert.rejects(
    () => expect(page.locator('#missing')).not.toHaveText('anything', { timeout: 150 }),
    /element not found/,
  )

  await assert.rejects(
    () => expect(page).toHaveURL('/never', { timeout: 150 }),
    /Expected:.*\/never[\s\S]*Received:/,
  )

  await page.close()
} finally {
  await browser.close()
  server.close()
}
console.log('expect ok')
