// Run with: node --test tests/services/etl-status.test.ts
// (Node 22+ strips TypeScript types automatically; matches the runner used by
// tests/services/format.test.ts and tests/services/filters.test.ts.)
//
// Wave 4F — coverage for the ETL status fetcher + presentation helpers that
// power EtlStatusBadge. The component itself isn't render-tested (no DOM
// runner in this project); we lock the data-shape contract and the
// health → colour / text-formatter mapping that drives the UI.

import { test } from 'node:test'
import assert from 'node:assert/strict'

import {
  EtlBackfillInProgressError,
  EtlPipelineNotReadyError,
  etlHealthColor,
  formatEtlBadgeText,
  formatLagDuration,
  getEtlStatus,
  triggerEtlBackfill,
} from '../../src/services/api.ts'
import type { EtlHealth, EtlStatusResponse } from '../../src/types/api.ts'

// ---------------------------------------------------------------------------
// Helpers — minimal `fetch` stub. We swap globalThis.fetch for the duration of
// each test and restore it afterwards. Matches the pattern used elsewhere in
// the project (no msw / no jest-fetch-mock dep).
// ---------------------------------------------------------------------------

interface MockResponse {
  ok: boolean
  status: number
  statusText: string
  json: () => Promise<unknown>
  text: () => Promise<string>
}

function mockResponse(body: unknown, status = 200): MockResponse {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: status === 200 ? 'OK' : status === 404 ? 'Not Found' : 'Error',
    json: async () => body,
    text: async () => (typeof body === 'string' ? body : JSON.stringify(body)),
  }
}

function withFetch(impl: (input: string, init?: RequestInit) => Promise<MockResponse>): () => void {
  const original = (globalThis as { fetch?: unknown }).fetch
  ;(globalThis as { fetch: unknown }).fetch = impl as unknown as typeof fetch
  return () => {
    ;(globalThis as { fetch: unknown }).fetch = original as typeof fetch
  }
}

// ---------------------------------------------------------------------------
// Sample payload — every status field populated. Reused across tests.
// ---------------------------------------------------------------------------

const sampleStatus: EtlStatusResponse = {
  watcher: {
    enabled: true,
    running: true,
    last_refresh_ts: '2026-05-04T12:00:00Z',
    seconds_since_refresh: 7,
    events_in_last_cycle: 0,
  },
  marts: {
    daily_mart: { watermark: 1234, row_count: 100, last_refresh_ts: '2026-05-04T12:00:00Z' },
    session_mart: { watermark: 1234, row_count: 50, last_refresh_ts: '2026-05-04T12:00:00Z' },
  },
  events: {
    total: 1234,
    max_id: 1234,
    by_provider: { claude: 800, codex: 300, cursor: 134 },
    by_cost_source: { actual: 1100, estimated: 134 },
  },
  lag_seconds: 7,
  health: 'live',
  current_job: null,
  last_job: null,
}

// ---------------------------------------------------------------------------
// Fetcher: getEtlStatus — happy path parses the contract.
// ---------------------------------------------------------------------------

test('getEtlStatus parses the response shape', async () => {
  const restore = withFetch(async (url) => {
    assert.equal(url, '/api/etl/status')
    return mockResponse(sampleStatus)
  })
  try {
    const data = await getEtlStatus()
    assert.equal(data.health, 'live')
    assert.equal(data.events.total, 1234)
    assert.equal(data.events.by_provider.claude, 800)
    assert.equal(data.watcher.running, true)
    assert.equal(data.marts.daily_mart!.row_count, 100)
    assert.equal(data.lag_seconds, 7)
  } finally {
    restore()
  }
})

test('getEtlStatus throws EtlPipelineNotReadyError on 404', async () => {
  const restore = withFetch(async () => mockResponse({ detail: 'not found' }, 404))
  try {
    await assert.rejects(getEtlStatus, EtlPipelineNotReadyError)
  } finally {
    restore()
  }
})

test('getEtlStatus throws a generic Error on 500', async () => {
  const restore = withFetch(async () => mockResponse('server explosion', 500))
  try {
    await assert.rejects(getEtlStatus, (err) => {
      assert.ok(err instanceof Error)
      assert.ok(!(err instanceof EtlPipelineNotReadyError))
      assert.match(err.message, /500/)
      return true
    })
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// Fetcher: triggerEtlBackfill — POST + force flag + 404 handling.
// ---------------------------------------------------------------------------

// 202 success body — populated by the route's process-local job slot.
const sampleAcceptedBody = {
  job_id: 'd41d8cd98f00b204e9800998ecf8427e',
  started_at: '2026-05-06T12:34:56+00:00',
}

test('triggerEtlBackfill POSTs to /api/etl/backfill with the force flag', async () => {
  let captured: { url: string; init: RequestInit | undefined } | null = null
  const restore = withFetch(async (url, init) => {
    captured = { url, init }
    return mockResponse(sampleAcceptedBody, 202)
  })
  try {
    const res = await triggerEtlBackfill(true)
    assert.equal(res.job_id, sampleAcceptedBody.job_id)
    assert.equal(res.started_at, sampleAcceptedBody.started_at)
    assert.ok(captured, 'fetch should have been called')
    const cap = captured! as { url: string; init: RequestInit | undefined }
    assert.equal(cap.url, '/api/etl/backfill')
    assert.equal(cap.init?.method, 'POST')
    const body = JSON.parse(cap.init?.body as string)
    assert.equal(body.force, true)
  } finally {
    restore()
  }
})

test('triggerEtlBackfill defaults force=false', async () => {
  const restore = withFetch(async (_url, init) => {
    const body = JSON.parse(init?.body as string)
    assert.equal(body.force, false)
    return mockResponse(sampleAcceptedBody, 202)
  })
  try {
    await triggerEtlBackfill()
  } finally {
    restore()
  }
})

test('triggerEtlBackfill on 404 raises EtlPipelineNotReadyError with CLI hint', async () => {
  const restore = withFetch(async () => mockResponse({ detail: 'no such route' }, 404))
  try {
    await assert.rejects(triggerEtlBackfill, (err) => {
      assert.ok(err instanceof EtlPipelineNotReadyError)
      assert.match(err.message, /stax etl backfill/)
      return true
    })
  } finally {
    restore()
  }
})

test('triggerEtlBackfill on 409 raises EtlBackfillInProgressError with the job id', async () => {
  const otherJob = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
  const restore = withFetch(async () =>
    mockResponse({ error: 'backfill_in_progress', job_id: otherJob }, 409),
  )
  try {
    await assert.rejects(triggerEtlBackfill, (err) => {
      assert.ok(err instanceof EtlBackfillInProgressError)
      assert.equal((err as EtlBackfillInProgressError).jobId, otherJob)
      return true
    })
  } finally {
    restore()
  }
})

test('triggerEtlBackfill on 409 with malformed body still raises EtlBackfillInProgressError', async () => {
  // The route is supposed to send {job_id}; if it doesn't, the fetcher
  // should still raise the conflict signal with a placeholder rather
  // than a generic Error so the UI can show the right message.
  const restore = withFetch(async () => ({
    ok: false,
    status: 409,
    statusText: 'Conflict',
    json: async () => {
      throw new Error('not json')
    },
    text: async () => '',
  }))
  try {
    await assert.rejects(triggerEtlBackfill, (err) => {
      assert.ok(err instanceof EtlBackfillInProgressError)
      assert.equal((err as EtlBackfillInProgressError).jobId, 'unknown')
      return true
    })
  } finally {
    restore()
  }
})

// ---------------------------------------------------------------------------
// Health → badge colour mapping. Locks the four colours stated in the spec
// (live=green, syncing=blue, stale=yellow, error=red) plus the pulsing flag
// (only syncing pulses; the others are static).
// ---------------------------------------------------------------------------

const colourCases: Array<[EtlHealth, 'green' | 'blue' | 'yellow' | 'red', boolean]> = [
  ['live', 'green', false],
  ['syncing', 'blue', true],
  ['stale', 'yellow', false],
  ['error', 'red', false],
]

for (const [health, badge, pulse] of colourCases) {
  test(`etlHealthColor(${health}) → ${badge} (pulse=${pulse})`, () => {
    const c = etlHealthColor(health)
    assert.equal(c.badge, badge)
    assert.equal(c.pulse, pulse)
    assert.match(c.dot, new RegExp(`bg-${badge}-500`))
  })
}

// ---------------------------------------------------------------------------
// formatLagDuration — single-unit compact formatter.
// ---------------------------------------------------------------------------

const lagCases: Array<[number | null | undefined, string]> = [
  [0, '0s'],
  [7, '7s'],
  [59, '59s'],
  [60, '1m'],
  [120, '2m'],
  [3599, '59m'],
  [3600, '1h'],
  [3600 * 23, '23h'],
  [3600 * 24, '1d'],
  [3600 * 24 * 7, '7d'],
  [null, '—'],
  [undefined, '—'],
  [-1, '—'],
  [Number.NaN, '—'],
  [Number.POSITIVE_INFINITY, '—'],
]

for (const [secs, expected] of lagCases) {
  test(`formatLagDuration(${secs}) → ${JSON.stringify(expected)}`, () => {
    assert.equal(formatLagDuration(secs), expected)
  })
}

// ---------------------------------------------------------------------------
// formatEtlBadgeText — sentence-style label per health state.
// ---------------------------------------------------------------------------

test('formatEtlBadgeText: live shows synced-ago duration', () => {
  const text = formatEtlBadgeText({ ...sampleStatus, health: 'live' })
  assert.match(text, /Live \(synced 7s ago\)/)
})

test('formatEtlBadgeText: syncing shows event backlog (singular)', () => {
  const text = formatEtlBadgeText({
    ...sampleStatus,
    health: 'syncing',
    watcher: { ...sampleStatus.watcher, events_in_last_cycle: 1 },
  })
  assert.equal(text, 'Syncing (1 event behind)')
})

test('formatEtlBadgeText: syncing shows event backlog (plural)', () => {
  const text = formatEtlBadgeText({
    ...sampleStatus,
    health: 'syncing',
    watcher: { ...sampleStatus.watcher, events_in_last_cycle: 12 },
  })
  assert.equal(text, 'Syncing (12 events behind)')
})

test('formatEtlBadgeText: stale renders short lag', () => {
  const text = formatEtlBadgeText({ ...sampleStatus, health: 'stale', lag_seconds: 120 })
  assert.equal(text, 'Stale by 2m')
})

test('formatEtlBadgeText: stale renders day-scale lag', () => {
  const text = formatEtlBadgeText({
    ...sampleStatus,
    health: 'stale',
    lag_seconds: 3600 * 24,
  })
  assert.equal(text, 'Stale by 1d')
})

test('formatEtlBadgeText: error has the canonical pointer to /etl/status', () => {
  // Generic error path — no recently failed last_job. The badge falls
  // back to the canonical pointer so the user knows where to look.
  const text = formatEtlBadgeText({ ...sampleStatus, health: 'error' })
  assert.match(text, /ETL error/)
  assert.match(text, /\/etl\/status/)
})

// ---------------------------------------------------------------------------
// last_job — surfacing recent backfill outcomes. The assembler escalates
// `health` to "error" while a failed last_job is inside its TTL window, and
// the badge text reflects that more specific failure rather than the generic
// "ETL error" message so the user gets one click closer to the cause.
// ---------------------------------------------------------------------------

test('EtlStatusResponse round-trips a failed last_job block', async () => {
  const failed: EtlStatusResponse = {
    ...sampleStatus,
    health: 'error',
    last_job: {
      job_id: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
      started_at: '2026-05-06T12:00:00+00:00',
      completed_at: '2026-05-06T12:00:42+00:00',
      force: true,
      status: 'failed',
      error: 'connection refused: 5/5',
    },
  }
  const restore = withFetch(async () => mockResponse(failed))
  try {
    const data = await getEtlStatus()
    assert.equal(data.last_job?.status, 'failed')
    assert.equal(data.last_job?.error, 'connection refused: 5/5')
    assert.equal(data.last_job?.force, true)
    assert.equal(data.health, 'error')
  } finally {
    restore()
  }
})

test('EtlStatusResponse round-trips a complete last_job with no error key', async () => {
  const complete: EtlStatusResponse = {
    ...sampleStatus,
    last_job: {
      job_id: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
      started_at: '2026-05-06T12:00:00+00:00',
      completed_at: '2026-05-06T12:00:42+00:00',
      force: false,
      status: 'complete',
    },
  }
  const restore = withFetch(async () => mockResponse(complete))
  try {
    const data = await getEtlStatus()
    assert.equal(data.last_job?.status, 'complete')
    // `error` is optional on the wire — successful completions omit it
    // and consumers branch on `status === 'failed'`, never on the
    // presence of the `error` key alone.
    assert.equal(data.last_job?.error ?? null, null)
  } finally {
    restore()
  }
})

test('formatEtlBadgeText: failed last_job surfaces the job id', () => {
  const failed: EtlStatusResponse = {
    ...sampleStatus,
    health: 'error',
    last_job: {
      job_id: 'd41d8cd98f00b204e9800998ecf8427e',
      started_at: '2026-05-06T12:00:00+00:00',
      completed_at: '2026-05-06T12:00:42+00:00',
      force: false,
      status: 'failed',
      error: 'connection refused',
    },
  }
  const text = formatEtlBadgeText(failed)
  assert.match(text, /Backfill failed/)
  // First 8 hex chars of the job id should appear so the operator can
  // correlate with the server log line.
  assert.match(text, /d41d8cd9/)
  // The generic "/etl/status" hint must NOT appear when we have a more
  // specific message — that's the whole point of this branch.
  assert.doesNotMatch(text, /\/etl\/status/)
})

test('formatEtlBadgeText: error without last_job falls back to generic message', () => {
  // health=error can also come from a dead-watcher + lag combination.
  // When last_job isn't a recently failed run we keep the canonical
  // generic message so the user is pointed at the route for context.
  const dead: EtlStatusResponse = { ...sampleStatus, health: 'error', last_job: null }
  const text = formatEtlBadgeText(dead)
  assert.match(text, /ETL error/)
  assert.match(text, /\/etl\/status/)
})

test('formatEtlBadgeText: error with a last_job that is *complete* still uses generic msg', () => {
  // A successful last_job alongside health=error means the failure
  // came from somewhere else (watcher/lag) — don't claim a backfill
  // failed when it didn't.
  const generic: EtlStatusResponse = {
    ...sampleStatus,
    health: 'error',
    last_job: {
      job_id: 'cccccccccccccccccccccccccccccccc',
      started_at: '2026-05-06T12:00:00+00:00',
      completed_at: '2026-05-06T12:00:42+00:00',
      force: false,
      status: 'complete',
    },
  }
  const text = formatEtlBadgeText(generic)
  assert.match(text, /ETL error/)
  assert.match(text, /\/etl\/status/)
})
