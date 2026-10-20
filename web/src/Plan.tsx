import { useState } from 'react'
import { describeSource, pct, useApi, type Plan } from './api'
import { Status } from './components'

const PVP_KEY = 'plan.includePvp'

function loadPvp(): boolean {
  try { return localStorage.getItem(PVP_KEY) === '1' } catch { return false }
}

export default function PlanPage() {
  const [includePvp, setIncludePvp] = useState(loadPvp)
  const { data, error } = useApi<Plan>(`/plan?include_pvp=${includePvp}`)
  const [view, setView] = useState<'source' | 'need'>('source')

  function togglePvp(on: boolean) {
    setIncludePvp(on)
    try { localStorage.setItem(PVP_KEY, on ? '1' : '0') } catch { /* private mode: not remembered */ }
  }

  return (
    <>
      <h1>Farm plan</h1>
      <div className="toggle" role="group" aria-label="Plan view">
        <button type="button" aria-pressed={view === 'source'} onClick={() => setView('source')}>What to run</button>
        <button type="button" aria-pressed={view === 'need'} onClick={() => setView('need')}>What I need</button>
      </div>
      <label className="inline">
        <input type="checkbox" checked={includePvp} onChange={e => togglePvp(e.target.checked)} />
        Include PvP (Conclave) rewards
      </label>
      <Status error={error} loading={!data} />
      {data && data.by_need.length === 0 && data.unsourced.length === 0 && (
        <p className="muted">Nothing to farm. Add items to your wishlist.</p>
      )}

      {data && view === 'source' && (
        <ul className="rows">
          {data.by_source.map(g => (
            <li key={`${g.kind}|${g.name}|${g.rotation ?? ''}`} className={g.vaulted ? 'vaulted' : undefined}>
              <div className="row-head">
                <span className="badge">{g.kind}</span>
                <strong className="grow">{g.name}{g.rotation && ` — Rotation ${g.rotation}`}</strong>
                {g.vaulted && <span className="badge warn">Vaulted</span>}
                {g.pvp && <span className="badge">PvP</span>}
                {g.best_chance != null && <span className="nowrap">{pct(g.best_chance)}</span>}
              </div>
              <ul className="chips" aria-label="Covers">
                {g.covers.map(c => (
                  <li key={c.item_id}>{c.name}{c.via && ` via ${c.via}`}{c.chance != null && ` (${pct(c.chance)})`}</li>
                ))}
              </ul>
            </li>
          ))}
        </ul>
      )}

      {data && view === 'need' && (
        <ul className="rows">
          {data.by_need.map(n => (
            <li key={n.item.id}>
              <div className="row-head">
                <strong className="grow">{n.item.name}</strong>
                <span className="nowrap">{n.have} / {n.needed}</span>
              </div>
              <p className="muted">For: {n.for.map(f => f.name).join(', ')}</p>
              <ul className="sources">
                {n.sources.map((s, i) => <li key={i}><span className="badge">{s.kind}</span> {describeSource(s)}</li>)}
              </ul>
            </li>
          ))}
        </ul>
      )}

      {data && data.unsourced.length > 0 && (
        <section>
          <h2>No farmable source on wiki</h2>
          <ul className="rows">
            {data.unsourced.map(n => (
              <li key={n.item.id}>
                <div className="row-head">
                  <span className="grow">
                    {n.item.wiki_url ? <a href={n.item.wiki_url} target="_blank" rel="noreferrer">{n.item.name}</a> : n.item.name}
                  </span>
                  <span className="nowrap">{n.have} / {n.needed}</span>
                </div>
                <p className="muted">For: {n.for.map(f => f.name).join(', ')}</p>
              </li>
            ))}
          </ul>
        </section>
      )}
    </>
  )
}
