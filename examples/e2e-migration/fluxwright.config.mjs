import { defineConfig } from 'fluxwright/test'

export default defineConfig({
  testDir: './tests',
  workers: 1,
  retries: 0,
  timeout: 30_000,
  outputDir: './test-results',
  reportDir: './fluxwright-report',
  reporter: ['list', 'html'],
  use: {
    headless: true,
    screenshot: 'only-on-failure',
    trace: 'retain-on-failure',
  },
})
