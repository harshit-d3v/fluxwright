// 1-line migration from Playwright:
//   - import { test, expect } from '@playwright/test'
//   + import { test, expect } from 'fluxwright/test'
import { test, expect } from 'fluxwright/test'
import http from 'node:http'
import { once } from 'node:events'

const STOREFRONT_HTML = `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>FluxShop Checkout</title>
</head>
<body>
  <main>
    <h1>FluxShop Storefront</h1>

    <section aria-label="Catalog">
      <article data-testid="product-keyboard">
        <h2>Mechanical Keyboard</h2>
        <p>$120.00</p>
        <button id="add-keyboard" type="button">Add to cart</button>
      </article>
    </section>

    <section aria-label="Cart">
      <p>Items in cart: <span id="cart-count">0</span></p>
      <p>Total: <strong id="cart-total">$0.00</strong></p>

      <label for="promo-input">Promo code</label>
      <input id="promo-input" name="promo" placeholder="SAVE10" />
      <button id="apply-promo" type="button">Apply promo</button>

      <form id="checkout-form" novalidate>
        <label for="email-input">Email</label>
        <input id="email-input" name="email" type="email" />

        <label for="address-input">Shipping address</label>
        <input id="address-input" name="address" />

        <button id="place-order" type="submit">Place order</button>
      </form>

      <div id="error-banner" role="alert" hidden></div>
      <section id="confirmation" hidden>
        <h2>Order confirmed</h2>
        <p id="order-summary"></p>
      </section>
    </section>
  </main>

  <script>
    const UNIT_PRICE = 120
    const state = JSON.parse(sessionStorage.getItem('fluxshop') || '{"qty":0,"discount":0}')

    const countEl = document.getElementById('cart-count')
    const totalEl = document.getElementById('cart-total')
    const errorEl = document.getElementById('error-banner')
    const confirmEl = document.getElementById('confirmation')
    const summaryEl = document.getElementById('order-summary')

    function render() {
      sessionStorage.setItem('fluxshop', JSON.stringify(state))
      const total = state.qty * UNIT_PRICE * (1 - state.discount)
      countEl.textContent = String(state.qty)
      totalEl.textContent = '$' + total.toFixed(2)
    }

    function showError(msg) {
      errorEl.textContent = msg
      errorEl.hidden = false
    }

    document.getElementById('add-keyboard').addEventListener('click', () => {
      state.qty += 1
      errorEl.hidden = true
      render()
    })

    document.getElementById('apply-promo').addEventListener('click', () => {
      const code = document.getElementById('promo-input').value.trim().toUpperCase()
      if (code === 'SAVE10') {
        state.discount = 0.1
        errorEl.hidden = true
        render()
      } else {
        showError('Invalid promo code')
      }
    })

    document.getElementById('checkout-form').addEventListener('submit', (ev) => {
      ev.preventDefault()
      const email = document.getElementById('email-input').value.trim()
      const address = document.getElementById('address-input').value.trim()
      if (state.qty === 0) return showError('Your cart is empty')
      if (!email.includes('@')) return showError('Enter a valid email address')
      if (!address) return showError('Enter a shipping address')

      errorEl.hidden = true
      const total = (state.qty * UNIT_PRICE * (1 - state.discount)).toFixed(2)
      summaryEl.textContent = 'Order #FW-1001 for ' + email + ' — $' + total
      confirmEl.hidden = false
      history.pushState({}, '', '/confirmation?order=FW-1001')
    })

    render()
  </script>
</body>
</html>`

let fixtureServer
let baseUrl

test.describe('Storefront checkout flow', () => {
  test.beforeAll(async () => {
    fixtureServer = http.createServer((req, res) => {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' })
      res.end(STOREFRONT_HTML)
    })
    fixtureServer.listen(0, '127.0.0.1')
    await once(fixtureServer, 'listening')
    const { port } = fixtureServer.address()
    baseUrl = `http://127.0.0.1:${port}`
  })

  test.afterAll(async () => {
    if (fixtureServer) {
      fixtureServer.close()
    }
  })

  test('completes checkout with promo code and verifies confirmation', async ({ page }, testInfo) => {
    await page.goto(baseUrl)
    await expect(page).toHaveTitle('FluxShop Checkout')

    await page.getByRole('button', { name: 'Add to cart' }).click()
    await expect(page.locator('#cart-count')).toHaveText('1')
    await expect(page.locator('#cart-total')).toHaveText('$120.00')

    await page.getByLabel('Promo code').fill('SAVE10')
    await page.getByRole('button', { name: 'Apply promo' }).click()
    await expect(page.locator('#cart-total')).toHaveText('$108.00')

    await page.getByLabel('Email').fill('buyer@example.com')
    await page.getByLabel('Shipping address').fill('742 Evergreen Terrace')
    await page.getByRole('button', { name: 'Place order' }).click()

    await expect(page.getByRole('heading', { name: 'Order confirmed' })).toBeVisible()
    await expect(page.locator('#order-summary')).toContainText('Order #FW-1001')
    await expect(page.locator('#order-summary')).toContainText('$108.00')
    await expect(page).toHaveURL(/\/confirmation\?order=FW-1001$/)

    await testInfo.attach('checkout-summary.json', {
      contentType: 'application/json',
      body: JSON.stringify({ order: 'FW-1001', total: '$108.00' }),
    })
  })

  test('shows accessible validation errors for bad promo or missing email', async ({ page }) => {
    await page.goto(baseUrl)

    await page.getByLabel('Promo code').fill('EXPIRED')
    await page.getByRole('button', { name: 'Apply promo' }).click()
    await expect(page.getByRole('alert')).toHaveText('Invalid promo code')

    await page.getByRole('button', { name: 'Add to cart' }).click()
    await page.getByRole('button', { name: 'Place order' }).click()
    await expect(page.getByRole('alert')).toHaveText('Enter a valid email address')
  })

  test('preserves cart state across page reload', async ({ page }) => {
    await page.goto(baseUrl)
    await page.getByRole('button', { name: 'Add to cart' }).click()
    await expect(page.locator('#cart-count')).toHaveText('1')

    await page.reload()
    await expect(page.locator('#cart-count')).toHaveText('1')
    await expect(page.locator('#cart-total')).toHaveText('$120.00')
  })
})
