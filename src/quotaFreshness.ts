export interface QuotaRowForMerge {
  id: number
  source?: string | null
  updated_at?: string | null
  remaining_percent?: number | null
  windows?: { label: string; reset_at?: string | null }[]
}

function hasOlderReset(previous: QuotaRowForMerge, incoming: QuotaRowForMerge): boolean {
  const previousByLabel = new Map(previous.windows?.map((window) => [window.label, window]) ?? [])
  return incoming.windows?.some((window) => {
    const previousReset = Date.parse(previousByLabel.get(window.label)?.reset_at ?? '')
    const incomingReset = Date.parse(window.reset_at ?? '')
    return Number.isFinite(previousReset) && Number.isFinite(incomingReset) && incomingReset < previousReset - 60_000
  }) ?? false
}

export function mergeQuotaRows<T extends QuotaRowForMerge>(
  previousRows: T[],
  incomingRows: T[],
  activeObservedAt: Map<number, number>,
  now: number,
): T[] {
  const previousById = new Map(previousRows.map((row) => [row.id, row]))
  const presentIds = new Set(incomingRows.map((row) => row.id))

  for (const id of activeObservedAt.keys()) {
    if (!presentIds.has(id)) activeObservedAt.delete(id)
  }

  return incomingRows.map((incoming) => {
    if (incoming.source === 'active' && incoming.windows?.length) {
      activeObservedAt.set(incoming.id, now)
      return incoming
    }

    const activeAt = activeObservedAt.get(incoming.id)
    const previous = previousById.get(incoming.id)
    if (activeAt === undefined || !previous?.windows?.length) return incoming

    const cachedAt = Date.parse(incoming.updated_at ?? '')
    if (Number.isFinite(cachedAt) && cachedAt > activeAt && incoming.windows?.length
      && !hasOlderReset(previous, incoming)) {
      activeObservedAt.delete(incoming.id)
      return incoming
    }

    return {
      ...incoming,
      remaining_percent: previous.remaining_percent,
      windows: previous.windows,
      updated_at: previous.updated_at,
      source: previous.source,
    }
  })
}
