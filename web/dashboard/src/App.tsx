import { useEffect, useState } from 'react';

import { api, ApiError, whenSignedOut, type Me } from './api';
import { DatabasePage } from './pages/Database';
import { Databases } from './pages/Databases';
import { Keys } from './pages/Keys';
import { Usage } from './pages/Usage';
import { Link, useLocation } from './router';
import { ErrorNote, Spinner } from './ui';

function SignIn() {
  const back = location.pathname + location.search;
  return (
    <main className="sign-in">
      <h1>OctoPage</h1>
      <p className="lead">A SQL database kept in your own GitHub repository.</p>
      <a className="button button-primary" href={`/auth/github/login?redirect=${encodeURIComponent(back)}`}>
        Sign in with GitHub
      </a>
      <p className="muted small">
        The service reaches only the repositories you install its GitHub App on, with short-lived
        tokens. It stores no GitHub token of yours.
      </p>
    </main>
  );
}

const NAV = [
  { href: '/', label: 'Databases', match: (p: string) => p === '/' || p.startsWith('/databases') },
  { href: '/keys', label: 'API keys', match: (p: string) => p.startsWith('/keys') },
  { href: '/usage', label: 'Usage', match: (p: string) => p.startsWith('/usage') },
];

export function App() {
  const [me, setMe] = useState<Me | null | undefined>(undefined);
  const [error, setError] = useState<unknown>(null);
  const { path } = useLocation();

  useEffect(() => {
    whenSignedOut(() => setMe(null));
    api<Me>('GET', '/v1/me').then(setMe, (e) => {
      if (e instanceof ApiError && e.status === 401) setMe(null);
      else setError(e);
    });
  }, []);

  if (error) {
    return (
      <main className="page">
        <ErrorNote error={error} />
      </main>
    );
  }
  if (me === undefined) return <Spinner />;
  if (me === null) return <SignIn />;

  const signOut = async () => {
    await api('POST', '/auth/logout').catch(() => {});
    setMe(null);
  };
  const database = path.match(/^\/databases\/([^/]+)(?:\/([^/]+))?/);

  return (
    <>
      <header className="top">
        <Link to="/" className="brand">
          OctoPage
        </Link>
        <nav className="nav" aria-label="Main">
          {NAV.map((n) => (
            <Link key={n.href} to={n.href} className="nav-link" aria-current={n.match(path) ? 'page' : undefined}>
              {n.label}
            </Link>
          ))}
        </nav>
        <div className="who">
          <span data-testid="login">{me.login}</span>
          <button type="button" className="button button-small" onClick={signOut}>
            Sign out
          </button>
        </div>
      </header>
      <main className="page">
        {database ? (
          <DatabasePage key={database[1]} id={decodeURIComponent(database[1])} tab={database[2] ?? 'console'} />
        ) : path === '/' || path === '/databases' ? (
          <Databases me={me} />
        ) : path === '/keys' ? (
          <Keys />
        ) : path === '/usage' ? (
          <Usage />
        ) : (
          <>
            <h1>Not found</h1>
            <p>
              There is no page here. <Link to="/">Go to your databases.</Link>
            </p>
          </>
        )}
      </main>
    </>
  );
}
