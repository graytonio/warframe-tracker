import { useState } from 'react'
import { send, useApi, type InventoryRow } from './api'
import { CountInput, ItemSearch, Status } from './components'

export default function Inventory() {
  const { data, error, reload } = useApi<InventoryRow[]>('/inventory')
  const [actionError, setActionError] = useState<string>()
  const saved = () => { setActionError(undefined); reload() }

  return (
    <>
      <h1>Inventory</h1>
      <ItemSearch label="Add resource" category="resource"
        onPick={async item => {
          // Already tracked: don't reset its count to 0.
          if (data?.some(r => r.item.id === item.id)) return
          return send('PATCH', `/inventory/${item.id}`, { count: 0 }).then(reload, e => setActionError(String(e.message ?? e)))
        }} />
      {actionError && <p className="error" role="alert">{actionError}</p>}
      <Status error={error} loading={!data} />
      {data?.length === 0 && <p className="muted">No inventory yet.</p>}
      {data && data.length > 0 && (
        <div className="table-wrap">
          <table>
            <thead>
              <tr><th scope="col">Item</th><th scope="col">Count</th><th scope="col">Source</th><th scope="col">Updated</th></tr>
            </thead>
            <tbody>
              {data.map(r => (
                <tr key={r.item.id}>
                  <th scope="row">{r.item.name}</th>
                  <td><CountInput itemId={r.item.id} value={r.count} label={`${r.item.name} count`} onSaved={saved} onError={setActionError} /></td>
                  <td>{r.source}</td>
                  <td>{new Date(r.updated_at).toLocaleDateString()}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  )
}
