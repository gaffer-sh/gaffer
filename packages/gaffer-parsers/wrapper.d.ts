export function detect_format(content: string, filename: string): string
export function parse_report(content: string, filename: string): string
export function parse_coverage(content: string, filename: string): string
export function parse_mutation(content: string, filename: string): string
export function parse_findings(content: string, filename: string): string

export interface TraceNetwork {
  playwrightVersion: string | null
  requests: Array<{
    method: string
    url: string
    /** 0 for a network error */
    status: number
    /** ISO 8601 UTC with milliseconds */
    startedAt: string
    durationMs: number
    ids: Array<{ header: string, value: string, on: 'request' | 'response' }>
  }>
}
/** Throws on a zip that is not a Playwright trace or a trace version we do not know. */
export function parseTraceNetwork(zip: Uint8Array): TraceNetwork
