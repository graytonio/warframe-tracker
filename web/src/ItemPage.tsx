import { useState } from 'react'
import { useParams } from 'react-router-dom'
import { describeSource, send, useApi, type ItemDetail } from './api'
import { useAuth } from './auth'
import { Status, Tree } from './components'

export default function ItemPage() {
  const { id } = useParams()
  const auth = useAuth()
  const signedIn = auth.status === 'signedIn'
  const { data, error, reload } = useApi<ItemDetail>(`/items/${id}`)
  const [note, setNote] = useState<{ ok: boolean; text: string }>()

  if (!data) return <Status error={error} loading />
  const { item, tree, sources } = data
  const add = () => send('POST', '/wishlist', { item_id: item.id })
    .then(() => setNote({ ok: true, text: 'Added to your wishlist.' }), e => setNote({ ok: false, text: e.message }))

  return (
    <>
      <div className="row-head">
        {item.image_url && <img src={item.image_url} alt="" width={64} height={64} />}
        <h1 className="grow">{item.name} <span className="badge">{item.category}</span></h1>
        {signedIn && <button type="button" className="primary" onClick={add}>Add to wishlist</button>}
      </div>
      {note && <p className={note.ok ? 'muted' : 'error'} role={note.ok ? 'status' : 'alert'}>{note.text}</p>}
      {item.wiki_url && <p><a href={item.wiki_url}>Wiki page</a></p>}
      <h2>Build tree</h2>
      {tree.children.length > 0
        ? <ul className="tree">{tree.children.map((c, i) => (
            <Tree key={`${c.item.id}-${i}`} node={c} reload={reload} readOnly={!signedIn}
              onError={text => setNote({ ok: false, text })} />
          ))}</ul>
        : <p className="muted">No recipe on the wiki.</p>}
      <h2>Sources</h2>
      {sources.length === 0
        ? <p className="muted">No known drop sources.</p>
        : (
          <div className="table-wrap">
            <table>
              <thead><tr><th scope="col">Kind</th><th scope="col">Where</th></tr></thead>
              <tbody>
                {sources.map((s, i) => (
                  <tr key={i}>
                    <td>{s.kind}{s.pvp && ' (PvP)'}</td>
                    <td>{describeSource(s)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
    </>
  )
}
