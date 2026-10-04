import assert from 'node:assert/strict'
import test from 'node:test'
import { mergeQuotaRows } from '../src/quotaFreshness.ts'

interface Row {
  id: number
  name: string
  source: string
  updated_at: string | null
  remaining_percent: number
  windows: { label: string; remaining_percent: number; reset_at?: string }[]
}

const row = (id: number, remaining: number, updated: string | null, source: string): Row => ({
  id,
  name: `Account ${id}`,
  source,
  updated_at: updated,
  remaining_percent: remaining,
  windows: [{ label: '7d', remaining_percent: remaining }],
})

test('old passive data does not replace a manually refreshed account', () => {
  const observedAt = Date.parse('2026-10-01T01:00:00Z')
  const activeObservedAt = new Map<number, number>()
  const active = mergeQuotaRows([], [row(1, 100, '2026-10-01T01:00:00Z', 'active')], activeObservedAt, observedAt)
  const cached = mergeQuotaRows(active, [row(1, 12, '2026-09-30T20:00:00Z', 'cached')], activeObservedAt, observedAt + 30_000)
  assert.equal(cached[0].remaining_percent, 100)
  assert.equal(cached[0].source, 'active')
})

test('a genuinely newer cache replaces the active result for that account', () => {
  const observedAt = Date.parse('2026-10-01T01:00:00Z')
  const activeObservedAt = new Map([[1, observedAt]])
  const previous = [row(1, 100, '2026-10-01T01:00:00Z', 'active'), row(2, 40, null, 'cached')]
  const incoming = [row(1, 95, '2026-10-01T01:02:00Z', 'cached'), row(2, 35, null, 'cached')]
  const merged = mergeQuotaRows(previous, incoming, activeObservedAt, observedAt + 120_000)
  assert.equal(merged[0].remaining_percent, 95)
  assert.equal(merged[1].remaining_percent, 35)
  assert.equal(activeObservedAt.has(1), false)
})

test('missing cache timestamp cannot claim to be newer', () => {
  const observedAt = Date.parse('2026-10-01T01:00:00Z')
  const activeObservedAt = new Map([[1, observedAt]])
  const previous = [row(1, 100, '2026-10-01T01:00:00Z', 'active')]
  const cached = mergeQuotaRows(previous, [row(1, 12, null, 'cached')], activeObservedAt, observedAt + 30_000)
  assert.equal(cached[0].remaining_percent, 100)
})

test('a newer fetch timestamp cannot revive an older quota period', () => {
  const observedAt = Date.parse('2026-10-01T01:00:00Z')
  const activeObservedAt = new Map([[1, observedAt]])
  const active = row(1, 100, '2026-10-01T01:00:00Z', 'active')
  active.windows[0].reset_at = '2026-10-08T00:00:00Z'
  const stale = row(1, 12, '2026-10-01T01:02:00Z', 'cached')
  stale.windows[0].reset_at = '2026-10-01T00:00:00Z'
  const merged = mergeQuotaRows([active], [stale], activeObservedAt, observedAt + 120_000)
  assert.equal(merged[0].remaining_percent, 100)
  assert.equal(activeObservedAt.has(1), true)
})
