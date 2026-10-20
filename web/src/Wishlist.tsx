import { useState } from 'react'
import { send, useApi, type WishlistEntry } from './api'
import { ItemSearch, Status, Tree } from './components'

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
