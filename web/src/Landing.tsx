import { useNavigate, useSearchParams } from 'react-router-dom'
import { SignInButton } from './auth'
import { ItemSearch } from './components'

const NOTICES: Record<string, [string, string]> = {
  'login': ['cancelled', 'Sign-in cancelled.'],
  'expired': ['1', 'Session expired, please sign in again.'],
}

export default function Landing() {
  const navigate = useNavigate()
  const [params, setParams] = useSearchParams()
  const notices = Object.entries(NOTICES).filter(([k, [v]]) => params.get(k) === v)

  return (
    <>
      <h1>Warframe Tracker</h1>
      {notices.map(([k, [, text]]) => (
        <p key={k} className="banner" role="status">
          {text}{' '}
          <button type="button" className="link" onClick={() => setParams(p => { p.delete(k); return p })}>Dismiss</button>
        </p>
      ))}
      <p>Wishlist Warframe gear, track what you've built and farmed, and get a plan for the rest.</p>
      <p><SignInButton /></p>
      <h2>Browse the catalog</h2>
      <ItemSearch label="Search items" action="View" onPick={async item => navigate(`/items/${item.id}`)} />
    </>
  )
}
