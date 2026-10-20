import { NavLink, Route, Routes } from 'react-router-dom'
import { useApi, type Meta } from './api'
import Wishlist from './Wishlist'
import PlanPage from './Plan'
import Inventory from './Inventory'

export default function App() {
  const meta = useApi<Meta>('/meta')
  return (
    <>
      <header>
        <nav aria-label="Main">
          <NavLink to="/" end>Wishlist</NavLink>
          <NavLink to="/plan">Plan</NavLink>
          <NavLink to="/inventory">Inventory</NavLink>
        </nav>
      </header>
      {meta.data?.item_count === 0 && (
        <p className="banner" role="status">
          Game data not imported yet — run <code>warframe-tracker import</code>.
        </p>
      )}
      <main>
        <Routes>
          <Route path="/" element={<Wishlist />} />
          <Route path="/plan" element={<PlanPage />} />
          <Route path="/inventory" element={<Inventory />} />
          <Route path="*" element={<p>Page not found.</p>} />
        </Routes>
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
