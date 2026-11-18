// parseTraceNetwork: unzip a Playwright trace.zip here (fflate; the Rust crate
// has no inflate dependency) and hand the text to the WASM parser. Only the
// `.network` files are inflated in full; each `.trace` is inflated up to its
// first line (context-options), and resources/ are never inflated.

import { Unzip, UnzipInflate } from 'fflate'
import { parse_trace_network } from './pkg/gaffer_parsers_bg.js'

// The zip is customer-supplied and this runs in a 128 MiB Worker; a real 28 MiB trace had 7.5 MiB of .network.
export const MAX_INFLATED_BYTES = 64 * 1024 * 1024

export function parseTraceNetwork(zip) {
  const network = []
  const trace = []
  let inflated = 0
  const unzip = new Unzip()
  unzip.register(UnzipInflate)
  unzip.onfile = (file) => {
    const isNetwork = file.name.endsWith('.network')
    if (!isNetwork && !file.name.endsWith('.trace'))
      return
    const decoder = new TextDecoder()
    const parts = []
    file.ondata = (err, chunk, final) => {
      if (err)
        throw err
      inflated += chunk.length
      if (inflated > MAX_INFLATED_BYTES)
        throw new Error(`Trace inflates past ${MAX_INFLATED_BYTES} bytes of .network/.trace text`)
      parts.push(decoder.decode(chunk, { stream: !final }))
      if (isNetwork) {
        if (final)
          network.push(parts.join(''))
        return
      }
      const text = parts.join('')
      const newline = text.indexOf('\n')
      if (newline === -1 && !final)
        return
      trace.push(newline === -1 ? text : text.slice(0, newline))
      if (!final)
        file.terminate()
    }
    file.start()
  }
  unzip.push(zip, true)
  return JSON.parse(parse_trace_network(network.join('\n'), trace.join('\n')))
}
