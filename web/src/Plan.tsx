import { useState } from 'react'
import { pct, useApi, type Plan } from './api'
import { Status } from './components'

const PVP_KEY = 'plan.includePvp'
const VIEW_KEY = 'plan.view'
type View = 'location' | 'source'
const VIEWS: [View, string][] = [['location', 'Where to go'], ['source', 'What to run']]

function loadPvp(): boolean {
  try { return localStorage.getItem(PVP_KEY) === '1' } catch { return false }
}

function loadView(): View {
  try {
    const v = localStorage.getItem(VIEW_KEY)
    return VIEWS.some(([k]) => k === v) ? (v as View) : 'location'
  } catch { return 'location' }
}

export default function PlanPage() {
  const [includePvp, setIncludePvp] = useState(loadPvp)
  const { data, error } = useApi<Plan>(`/plan?include_pvp=${includePvp}`)
  const [view, setViewState] = useState<View>(loadView)

  function togglePvp(on: boolean) {
    setIncludePvp(on)
    try { localStorage.setItem(PVP_KEY, on ? '1' : '0') } catch { /* private mode: not remembered */ }
  }

  function setView(v: View) {
    setViewState(v)
    try { localStorage.setItem(VIEW_KEY, v) } catch { /* private mode: not remembered */ }
  }

  return (
    <>
      <h1>Farm plan</h1>
      <div className="toggle" role="group" aria-label="Plan view">
        {VIEWS.map(([v, label]) => (
          <button key={v} type="button" aria-pressed={view === v} onClick={() => setView(v)}>{label}</button>
        ))}
      </div>
      <label className="inline">
        <input type="checkbox" checked={includePvp} onChange={e => togglePvp(e.target.checked)} />
        Include PvP (Conclave) rewards
      </label>
      <Status error={error} loading={!data} />
      {data && data.by_need.length === 0 && data.unsourced.length === 0 && (
        <p className="muted">Nothing to farm. Add items to your wishlist.</p>
      )}

      {data && view === 'location' && data.by_need.length > 0 && data.by_location.length === 0 && (
        <p className="muted">No structured locations for your needs yet.</p>
      )}
      {data && view === 'location' && (
        <ul className="rows">
          {data.by_location.map(g => (
            <li key={g.place}>
              <details className="place">
                <summary>
                  <h3>{g.place}</h3>
                  <ul className="chips" aria-label="Covers">
                    {g.covers.map(c => <li key={c.item_id}>{c.name} — {c.via.join(', ')}</li>)}
                  </ul>
                  {g.nodes.length > 0 && <p className="muted">e.g. {g.nodes.join(', ')}</p>}
                </summary>
                <div className="table-wrap">
                  <table>
                    <caption className="sr-only">Where each material drops on {g.place}</caption>
                    <thead>
                      <tr><th scope="col">Site</th><th scope="col">Material</th><th scope="col">Rotation</th><th scope="col">Chance</th><th scope="col">Qty</th><th scope="col">Via</th></tr>
                    </thead>
                    <tbody>
                      {g.sites.map((s, i) => (
                        <tr key={i}>
                          <th scope="row">{s.site}</th>
                          <td>{s.name}</td>
                          <td>{s.rotation ?? '—'}</td>
                          <td className="nowrap">{s.chance != null ? pct(s.chance) : '—'}</td>
                          <td>{s.quantity != null ? `×${s.quantity}` : '—'}</td>
                          <td>{s.via ? `${s.via} relic` : s.kind === 'planet' ? 'region resource' : '—'}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </details>
            </li>
          ))}
        </ul>
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
