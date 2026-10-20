import { useEffect, useId, useState } from 'react'
import { api, send, type ItemSummary, type ProgressNode } from './api'

export function Status({ error, loading }: { error?: string; loading: boolean }) {
  if (error) return <p className="error" role="alert">Error: {error}</p>
  if (loading) return <p className="muted">Loading…</p>
  return null
}

const CATEGORIES = ['warframe', 'weapon', 'mod', 'arcane']

/** Debounced catalog search. Pass `category` to pin it; otherwise a category select is shown. */
export function ItemSearch({ label, category, onPick, action = 'Add' }: {
  label: string; category?: string; onPick: (item: ItemSummary) => Promise<void>; action?: string
}) {
  const id = useId()
  const [q, setQ] = useState('')
  const [cat, setCat] = useState(category ?? '')
  const [results, setResults] = useState<ItemSummary[]>([])
  const [error, setError] = useState<string>()

  useEffect(() => {
    if (!q.trim()) { setResults([]); return }
    let live = true
    const t = setTimeout(() => {
      const params = new URLSearchParams({ q, limit: '10' })
      if (cat) params.set('category', cat)
      api<ItemSummary[]>(`/items?${params}`).then(
        r => { if (live) { setResults(r); setError(undefined) } },
        e => { if (live) setError(e.message) },
      )
    }, 250)
    return () => { live = false; clearTimeout(t) }
  }, [q, cat])

  const pick = (item: ItemSummary) => onPick(item).then(() => setQ(''), e => setError(e.message))

  return (
    <section className="search" aria-label={label}>
      <div className="search-row">
        <label htmlFor={`${id}-q`} className="sr-only">{label}</label>
        <input id={`${id}-q`} type="search" placeholder={label} value={q} onChange={e => setQ(e.target.value)} />
        {category === undefined && (
          <>
            <label htmlFor={`${id}-cat`} className="sr-only">Category</label>
            <select id={`${id}-cat`} value={cat} onChange={e => setCat(e.target.value)}>
              <option value="">All</option>
              {CATEGORIES.map(c => <option key={c} value={c}>{c}</option>)}
            </select>
          </>
        )}
      </div>
      {error && <p className="error" role="alert">{error}</p>}
      {q.trim() && !error && results.length === 0 && <p className="muted">No matches.</p>}
      <ul className="results">
        {results.map(item => (
          <li key={item.id}>
            {item.image_url ? <img src={item.image_url} alt="" loading="lazy" width={40} height={40} /> : <span className="img-ph" />}
            <span className="grow">{item.name} <span className="badge">{item.category}</span></span>
            <button type="button" onClick={() => pick(item)} aria-label={`${action} ${item.name}`}>{action}</button>
          </li>
        ))}
      </ul>
    </section>
  )
}

export function CountInput({ itemId, value, label, onSaved, onError }: {
  itemId: number; value: number; label: string; onSaved: () => void; onError: (e: string) => void
}) {
  return (
    <input
      key={value} // remount when the server value changes so defaultValue resyncs
      type="number" min={0} step={1} inputMode="numeric" className="count"
      aria-label={label} defaultValue={value}
      onBlur={e => {
        const n = Number(e.target.value)
        if (!Number.isInteger(n) || n < 0) { e.target.value = String(value); return }
        if (n !== value) send('PATCH', `/inventory/${itemId}`, { count: n }).then(onSaved, err => onError(err.message))
      }}
    />
  )
}

/** A build tree. `readOnly` (signed out) shows quantities only: no checkboxes or counts. */
export function Tree({ node, reload, onError, readOnly }: {
  node: ProgressNode; reload: () => void; onError: (e: string) => void; readOnly?: boolean
}) {
  const { item } = node
  const isResource = item.category === 'resource'
  return (
    <li className={node.covered ? 'covered' : undefined}>
      <div className="node">
        {readOnly ? (
          <span className="grow">{item.name}{node.needed > 1 && ` ×${node.needed}`}</span>
        ) : isResource ? (
          <>
            <span className="grow">{item.name}</span>
            <span className="nowrap">
              <CountInput itemId={item.id} value={node.have} label={`${item.name} count`} onSaved={reload} onError={onError} />
              {' / '}{node.needed}
            </span>
          </>
        ) : (
          <label className="grow">
            <input
              type="checkbox" checked={node.obtained}
              onChange={e => send('PUT', `/progress/${item.id}`, { obtained: e.target.checked }).then(reload, err => onError(err.message))}
            />
            {' '}{item.name}{node.needed > 1 && ` ×${node.needed}`}
          </label>
        )}
      </div>
      {node.children.length > 0 && (
        <ul>{node.children.map((c, i) => <Tree key={`${c.item.id}-${i}`} node={c} reload={reload} onError={onError} readOnly={readOnly} />)}</ul>
      )}
    </li>
  )
}
