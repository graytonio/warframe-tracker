import { useId, useState } from 'react'
import { api, send, useApi, type NewToken, type TokenRow } from './api'
import { Status } from './components'

function NewTokenForm({ onCreated }: { onCreated: () => void }) {
  const id = useId()
  const [name, setName] = useState('')
  const [created, setCreated] = useState<NewToken>()
  const [error, setError] = useState<string>()
  const [copied, setCopied] = useState(false)

  const create = (e: React.FormEvent) => {
    e.preventDefault()
    api<NewToken>('/tokens', { method: 'POST', body: JSON.stringify({ name }) }).then(
      t => { setCreated(t); setName(''); setError(undefined); setCopied(false); onCreated() },
      e => setError(e.message),
    )
  }
  const copy = (text: string) => navigator.clipboard.writeText(text).then(() => setCopied(true), () => {})

  return (
    <>
      <form className="search-row" onSubmit={create}>
        <label htmlFor={id} className="sr-only">Token name</label>
        <input id={id} placeholder="Token name, e.g. gaming-pc" maxLength={64} required
          value={name} onChange={e => setName(e.target.value)} />
        <button type="submit">Create token</button>
      </form>
      {error && <p className="error" role="alert">{error}</p>}
      {created && (
        <div className="banner" role="status">
          <p>Token <strong>{created.name}</strong> created. Copy it now: it won't be shown again.</p>
          <p className="search-row">
            <code className="grow">{created.token}</code>
            <button type="button" onClick={() => copy(created.token)}>{copied ? 'Copied' : 'Copy'}</button>
          </p>
          <p>Run on your gaming PC:</p>
          <pre><code>WFT_URL={location.origin} WFT_TOKEN={created.token} wft-sync --once</code></pre>
        </div>
      )}
    </>
  )
}

function DeleteAccount() {
  const id = useId()
  const [confirm, setConfirm] = useState('')
  const [error, setError] = useState<string>()
  const del = () => send('DELETE', '/me').then(() => location.assign('/'), e => setError(e.message))

  return (
    <>
      <p>Deletes your wishlist, progress, inventory and sync tokens. This can't be undone.</p>
      <p className="search-row">
        <label htmlFor={id} className="sr-only">Type delete to confirm</label>
        <input id={id} placeholder="Type delete to confirm" value={confirm} onChange={e => setConfirm(e.target.value)} />
        <button type="button" className="danger" disabled={confirm !== 'delete'} onClick={del}>Delete my account</button>
      </p>
      {error && <p className="error" role="alert">{error}</p>}
    </>
  )
}

export default function Settings() {
  const { data, error, reload } = useApi<TokenRow[]>('/tokens')
  const [actionError, setActionError] = useState<string>()
  const revoke = (name: string) =>
    send('DELETE', `/tokens/${encodeURIComponent(name)}`).then(() => { setActionError(undefined); reload() }, e => setActionError(e.message))

  return (
    <>
      <h1>Settings</h1>
      <h2>Sync tokens</h2>
      <p className="muted">For <code>wft-sync</code>, which pushes your in-game inventory here.</p>
      <NewTokenForm onCreated={reload} />
      {actionError && <p className="error" role="alert">{actionError}</p>}
      <Status error={error} loading={!data} />
      {data?.length === 0 && <p className="muted">No tokens yet.</p>}
      <ul className="rows">
        {data?.map(t => (
          <li key={t.name} className="row-head">
            <span className="grow">
              <strong>{t.name}</strong>
              <span className="muted"> · created {new Date(t.created_at).toLocaleDateString()}
                {' · '}{t.last_used_at ? `last used ${new Date(t.last_used_at).toLocaleString()}` : 'never used'}</span>
            </span>
            <button type="button" className="danger" aria-label={`Revoke ${t.name}`} onClick={() => revoke(t.name)}>Revoke</button>
          </li>
        ))}
      </ul>
      <h2>Delete my account</h2>
      <DeleteAccount />
    </>
  )
}
