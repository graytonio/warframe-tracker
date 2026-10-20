import { useState } from 'react'
import { describeSource, api, send, useApi, type ProgressNode, type Source, type WishlistEntry } from './api'
import { ItemSearch, Status } from './components'

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

/** Where an item drops, fetched when the dropdown is first opened. */
function Sources({ itemId }: { itemId: number }) {
  const [sources, setSources] = useState<Source[]>()
  const [error, setError] = useState<string>()
  return (
    <details className="source-list" onToggle={e => {
      if (e.currentTarget.open && !sources) {
        api<{ sources: Source[] }>(`/items/${itemId}`).then(d => setSources(d.sources), err => setError(err.message))
      }
    }}>
      <summary>Where to get</summary>
      {error && <p className="error" role="alert">{error}</p>}
      {!sources && !error && <p className="muted">Loading…</p>}
      {sources?.length === 0 && <p className="muted">No farmable source on wiki.</p>}
      {sources && sources.length > 0 && (
        <ul className="sources">
          {sources.map((s, i) => <li key={i}><span className="badge">{s.kind}</span> {describeSource(s)}</li>)}
        </ul>
      )}
    </details>
  )
}

function Tree({ node, reload, onError }: { node: ProgressNode; reload: () => void; onError: (e: string) => void }) {
  const { item } = node
  const isResource = item.category === 'resource'
  return (
    <li className={node.covered ? 'covered' : undefined}>
      <div className="node">
        {isResource ? (
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
      {node.children.length === 0 && !node.covered && <Sources itemId={item.id} />}
      {node.children.length > 0 && (
        <ul>{node.children.map((c, i) => <Tree key={`${c.item.id}-${i}`} node={c} reload={reload} onError={onError} />)}</ul>
      )}
    </li>
  )
}

export default function Wishlist() {
  const { data, error, reload } = useApi<WishlistEntry[]>('/wishlist')
  const [actionError, setActionError] = useState<string>()
  const act = (p: Promise<void>) => p.then(() => { setActionError(undefined); reload() }, e => setActionError(e.message))

  return (
    <>
      <h1>Wishlist</h1>
      <ItemSearch label="Search items to add" onPick={item => send('POST', '/wishlist', { item_id: item.id }).then(reload)} />
      {actionError && <p className="error" role="alert">{actionError}</p>}
      <Status error={error} loading={!data} />
      {data?.length === 0 && <p className="muted">Your wishlist is empty. Search above to add something.</p>}
      <ul className="entries">
        {data?.map(w => {
          const isMod = w.item.category === 'mod'
          const isArcane = w.item.category === 'arcane'
          return (
            <li key={w.item.id} className="entry">
              {isMod || isArcane ? (
                <>
                  <div className="node">
                    <span className="grow">{w.item.name}</span>
                    {isMod ? (
                      <span className="nowrap">
                        <CountInput itemId={w.item.id} value={w.tree.have} label={`${w.item.name} count`} onSaved={reload} onError={setActionError} />
                        {' / '}1
                      </span>
                    ) : (
                      // Highest rank owned: synced from the game, or set by hand.
                      <span className="nowrap">
                        Rank{' '}
                        <input
                          key={w.tree.rank ?? 0}
                          type="number" min={0} max={w.tree.max_rank ?? undefined} step={1} inputMode="numeric" className="count"
                          aria-label={`${w.item.name} rank`} defaultValue={w.tree.rank ?? 0}
                          onBlur={e => {
                            const n = Number(e.target.value)
                            if (!Number.isInteger(n) || n < 0) { e.target.value = String(w.tree.rank ?? 0); return }
                            if (n !== (w.tree.rank ?? 0)) act(send('PUT', `/rank/${w.item.id}`, { rank: n }))
                          }}
                        />
                        {' / '}{w.tree.max_rank}
                      </span>
                    )}
                  </div>
                  {!w.tree.covered && <Sources itemId={w.item.id} />}
                </>
              ) : (
                <details>
                  <summary>
                    <span className="grow">{w.item.name}</span>
                    <progress max={100} value={w.progress_pct} aria-label={`${w.item.name} progress`} />
                    <span className="nowrap">{Math.round(w.progress_pct)}%</span>
                  </summary>
                  {w.tree.children.length > 0
                    ? <ul className="tree">{w.tree.children.map((c, i) => <Tree key={`${c.item.id}-${i}`} node={c} reload={reload} onError={setActionError} />)}</ul>
                    : <p className="muted">No recipe on the wiki.</p>}
                </details>
              )}
              <div className="actions">
                <button type="button" aria-label={`Mark ${w.item.name} done`}
                  onClick={() => act(send('DELETE', `/wishlist/${w.item.id}`))}>Done</button>
              </div>
            </li>
          )
        })}
      </ul>
    </>
  )
}
