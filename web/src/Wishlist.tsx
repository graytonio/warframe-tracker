import { useState } from 'react'
import { send, useApi, type ProgressNode, type WishlistEntry } from './api'
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
          const done = w.completed_at != null
          return (
            <li key={w.item.id} className={done ? 'entry done' : 'entry'}>
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
              <div className="actions">
                <button type="button" aria-pressed={done}
                  onClick={() => act(send('PATCH', `/wishlist/${w.item.id}`, { completed: !done }))}>
                  {done ? 'Done ✓' : 'Mark done'}
                </button>
                <button type="button" className="danger" aria-label={`Remove ${w.item.name}`}
                  onClick={() => act(send('DELETE', `/wishlist/${w.item.id}`))}>Remove</button>
              </div>
            </li>
          )
        })}
      </ul>
    </>
  )
}
