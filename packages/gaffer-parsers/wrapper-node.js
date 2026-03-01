// Node.js wrapper for dev mode.
// Loads WASM binary from disk via fs.readFileSync since Node.js cannot
// handle the ?module import suffix that bundlers (unwasm) transform.

import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  __wbg_set_wasm,
  __wbindgen_cast_0000000000000001,
  detect_format as _detect_format,
  parse_coverage as _parse_coverage,
  parse_report as _parse_report,
} from './pkg/gaffer_parsers_bg.js'

const __dirname = dirname(fileURLToPath(import.meta.url))
const wasmBuffer = readFileSync(join(__dirname, 'pkg', 'gaffer_parsers_bg.wasm'))
const wasmModule = new WebAssembly.Module(wasmBuffer)

const instance = new WebAssembly.Instance(wasmModule, {
  './gaffer_parsers_bg.js': { __wbindgen_cast_0000000000000001 },
})

__wbg_set_wasm(instance.exports)

export function detect_format(content, filename) {
  return _detect_format(content, filename)
}

export function parse_report(content, filename) {
  return _parse_report(content, filename)
}

export function parse_coverage(content, filename) {
  return _parse_coverage(content, filename)
}
