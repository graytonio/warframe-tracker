import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from 'react'
import { useNavigate } from 'react-router-dom'
import { api, setOnUnauthorized, type Me } from './api'

type Auth = { status: 'loading' } | { status: 'signedOut' } | { status: 'signedIn'; me: Me }

const Ctx = createContext<Auth>({ status: 'loading' })

export const useAuth = () => useContext(Ctx)

/** Full-page redirect: Discord sends the user back to `path`. */
export function signIn(path = location.pathname) {
  location.href = `/auth/discord/login?return_to=${encodeURIComponent(path)}`
}

export async function signOut() {
  await fetch('/auth/logout', { method: 'POST', credentials: 'same-origin' }).catch(() => {})
  location.assign('/')
}

export function AuthProvider({ children }: { children: ReactNode }) {
  const [auth, setAuth] = useState<Auth>({ status: 'loading' })
  const navigate = useNavigate()
  const signedIn = useRef(false)
  useEffect(() => { signedIn.current = auth.status === 'signedIn' }, [auth])

  useEffect(() => {
    api<Me>('/me').then(me => setAuth({ status: 'signedIn', me }), () => setAuth({ status: 'signedOut' }))
  }, [])

  // A 401 while signed in means the session ended: drop to the landing page once.
  // No-op otherwise, so a signed-out page can never loop.
  useEffect(() => {
    setOnUnauthorized(() => {
      if (!signedIn.current) return
      signedIn.current = false
      setAuth({ status: 'signedOut' })
      navigate('/?expired=1', { replace: true })
    })
    return () => setOnUnauthorized(undefined)
  }, [navigate])

  return <Ctx.Provider value={auth}>{children}</Ctx.Provider>
}

export function SignInButton() {
  return <button type="button" className="primary" onClick={() => signIn()}>Sign in with Discord</button>
}
