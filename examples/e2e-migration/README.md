# E2E Migration Starter (`@playwright/test` → `fluxwright/test`)

Realistic self-contained E2E checkout test suite demonstrating a **1-line import swap** from Playwright Test to Fluxwright's native test runner, complete with locators, `expect` assertions, failure screenshots, HTML reports, and traces.

## The 1-Line Swap

```diff
- import { test, expect } from '@playwright/test'
+ import { test, expect } from 'fluxwright/test'
```

## Run

```bash
cd examples/e2e-migration
npm install
npm test
```

Run with traces enabled for every test:

```bash
npx fluxwright-test --trace=on
```

The test file spins up its own local HTTP fixture storefront (`127.0.0.1:0`), so no external server or internet access is required.
