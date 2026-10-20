import type { ReactNode } from 'react'
import { Link, Navigate, NavLink, Route, Routes } from 'react-router-dom'
import { useApi, type Meta } from './api'
import { SignInButton, signOut, useAuth } from './auth'
import Wishlist from './Wishlist'
import PlanPage from './Plan'
import Inventory from './Inventory'
import Landing from './Landing'
import ItemPage from './ItemPage'
import Settings from './Settings'

/** Signed-out visitors go to the landing page. */
function Private({ children }: { children: ReactNode }) {
  return useAuth().status === 'signedIn' ? children : <Navigate to="/" replace />
}

function Account() {
  const auth = useAuth()
  if (auth.status !== 'signedIn') return <SignInButton />
  const { me } = auth
  return (
    <span className="account">
      {me.avatar_url && <img src={me.avatar_url} alt="" width={28} height={28} className="avatar" />}
      <span>{me.display_name}</span>
      <Link to="/settings">Settings</Link>
      <button type="button" onClick={signOut}>Sign out</button>
    </span>
  )
}

export default function App() {
  const meta = useApi<Meta>('/meta')
  const auth = useAuth()
  const signedIn = auth.status === 'signedIn'
  return (
    <>
      <header>
        <nav aria-label="Main">
          {signedIn ? (
            <>
              <NavLink to="/" end>Wishlist</NavLink>
              <NavLink to="/plan">Plan</NavLink>
              <NavLink to="/inventory">Inventory</NavLink>
              <NavLink to="/settings">Settings</NavLink>
            </>
          ) : <NavLink to="/" end>Catalog</NavLink>}
          <span className="grow" />
          {auth.status !== 'loading' && <Account />}
        </nav>
      </header>
      {meta.data?.item_count === 0 && (
        <p className="banner" role="status">
          Game data not imported yet — run <code>warframe-tracker import</code>.
        </p>
      )}
      <main>
        {auth.status === 'loading' ? <p className="muted">Loading…</p> : (
          <Routes>
            <Route path="/" element={signedIn ? <Wishlist /> : <Landing />} />
            <Route path="/items/:id" element={<ItemPage />} />
            <Route path="/plan" element={<Private><PlanPage /></Private>} />
            <Route path="/inventory" element={<Private><Inventory /></Private>} />
            <Route path="/settings" element={<Private><Settings /></Private>} />
            <Route path="*" element={<p>Page not found.</p>} />
          </Routes>
        )}
      </main>
      <footer>
        <p>
          Data from the official <a href="https://wiki.warframe.com/">Warframe Wiki</a> (CC BY-SA).
          {' '}Last import: {meta.error ? 'unavailable'
            : meta.data ? (meta.data.last_import_at ? new Date(meta.data.last_import_at).toLocaleString() : 'never')
            : '…'}
        </p>
      </footer>
    </>
  )
}
