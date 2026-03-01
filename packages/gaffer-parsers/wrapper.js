// Wrapper that bypasses unwasm's broken wasm-bindgen handling.
// See: https://github.com/unjs/unwasm/issues/21
//      https://github.com/nitrojs/nitro/issues/3089
//
// The ?module suffix tells unwasm to emit the .wasm file as-is and return a
// raw WebAssembly.Module instead of trying (and failing) to resolve
// wasm-bindgen's circular JS imports.

import {
  __wbg_set_wasm,
  __wbindgen_cast_0000000000000001,
  detect_format as _detect_format,
  parse_coverage as _parse_coverage,
  parse_report as _parse_report,
} from './pkg/gaffer_parsers_bg.js'
import wasmModule from './pkg/gaffer_parsers_bg.wasm?module'

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
