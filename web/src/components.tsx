import { useEffect, useId, useState } from 'react'
import { api, type ItemSummary } from './api'

export function Status({ error, loading }: { error?: string; loading: boolean }) {
  if (error) return <p className="error" role="alert">Error: {error}</p>
  if (loading) return <p className="muted">Loading…</p>
  return null
}

const CATEGORIES = ['warframe', 'weapon', 'mod', 'arcane']

/** Debounced catalog search. Pass `category` to pin it; otherwise a category select is shown. */
export function ItemSearch({ label, category, onPick }: {
  label: string; category?: string; onPick: (item: ItemSummary) => Promise<void>
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
            <button type="button" onClick={() => pick(item)} aria-label={`Add ${item.name}`}>Add</button>
          </li>
        ))}
      </ul>
    </section>
  )
}
