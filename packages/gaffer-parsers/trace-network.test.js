// End-to-end over the real trace.zip fixtures: fflate unzip + WASM parse.
// Run after `pnpm --filter @gaffer/parsers build`: `pnpm --filter @gaffer/parsers test:js`.

import { readFileSync } from 'node:fs'
import { zipSync } from 'fflate'
import { expect, it } from 'vitest'
import { MAX_INFLATED_BYTES } from './trace-network.js'
import { parseTraceNetwork } from './wrapper-node.js'

const fixture = version => readFileSync(new URL(`tests/fixtures/playwright-trace/${version}/trace.zip`, import.meta.url))

for (const [version, playwrightVersion] of [['1.57.0', '1.57.0'], ['1.49.1', null]]) {
  it(`parses the Playwright ${version} trace.zip`, () => {
    const parsed = parseTraceNetwork(fixture(version))
    expect(parsed.playwrightVersion).toBe(playwrightVersion)
    expect(parsed.requests.map(r => [r.method, r.status])).toEqual([['GET', 200], ['GET', 0], ['GET', 200], ['POST', 500]])
    expect(parsed.requests[2].ids.map(id => `${id.on}:${id.header}`)).toEqual(['request:x-request-id', 'request:traceparent', 'response:x-request-id'])
  })
}

it('throws on bytes that are not a trace zip', () => {
  expect(() => parseTraceNetwork(new TextEncoder().encode('not a zip'))).toThrow(/Not a Playwright trace/)
})

it('throws before inflating a .network past the size cap', () => {
  const bomb = zipSync({ '0-trace.network': new Uint8Array(MAX_INFLATED_BYTES + 1) })
  expect(() => parseTraceNetwork(bomb)).toThrow(/inflates past/)
})
