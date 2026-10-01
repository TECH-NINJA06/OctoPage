import { useState, type FormEvent } from 'react';

import { api, ApiError, branchName, shortSha, type DatabaseInfo } from '../api';
import { Link } from '../router';
import { ErrorNote, Spinner, Tabs, useAction, useLoad } from '../ui';
import { Branches } from './Branches';
import { Console } from './Console';
import { History } from './History';
import { SettingsTab } from './Settings';
import { Tables } from './Tables';

function Unlock({ id, onUnlocked }: { id: string; onUnlocked: () => void }) {
  const [passphrase, setPassphrase] = useState('');
  const action = useAction();
  const submit = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      await api('POST', `/v1/databases/${id}/unlock`, { passphrase });
      onUnlocked();
    });
  };
  return (
    <form className="card form" onSubmit={submit}>
      <h2>Unlock the database</h2>
      <p>
        It is encrypted with a passphrase, which the service keeps only in memory: give it again after
        the service restarts.
      </p>
      <label>
        Passphrase
        <input type="password" value={passphrase} onChange={(e) => setPassphrase(e.target.value)} required autoFocus />
      </label>
      <ErrorNote error={action.error} />
      <div className="actions">
        <button className="button button-primary" disabled={action.busy}>
          Unlock
        </button>
      </div>
    </form>
  );
}

const TABS = [
  { id: 'console', label: 'SQL' },
  { id: 'tables', label: 'Tables' },
  { id: 'history', label: 'History' },
  { id: 'branches', label: 'Branches' },
  { id: 'settings', label: 'Size and settings' },
];

export function DatabasePage({ id, tab }: { id: string; tab: string }) {
  const info = useLoad(() => api<DatabaseInfo>('GET', `/v1/databases/${encodeURIComponent(id)}`), [id]);
  const locked = info.error instanceof ApiError && info.error.code === 'locked';

  if (info.loading && !info.data) return <Spinner />;
  if (locked) return <Unlock id={id} onUnlocked={info.reload} />;
  if (!info.data) {
    return (
      <>
        <ErrorNote error={info.error} />
        <Link to="/">Back to your databases</Link>
      </>
    );
  }
  const db = info.data;
  const base = `/databases/${encodeURIComponent(id)}`;

  return (
    <>
      <div className="page-head">
        <div>
          <p className="crumbs">
            <Link to="/">Databases</Link> /
          </p>
          <h1>
            {db.repository} <span className="muted">· {branchName(db.branch)}</span>
          </h1>
          <p className="muted small">
            Head <code title={db.head}>{shortSha(db.head)}</code> · {db.page_size ? `${db.page_size / 1024} KB pages` : ''} ·{' '}
            {db.encryption === 'none' ? 'not encrypted' : 'encrypted'}
          </p>
        </div>
      </div>
      <Tabs tabs={TABS.map((t) => ({ ...t, href: `${base}/${t.id}` }))} current={tab} />
      <section className="tab-panel">
        {tab === 'console' ? (
          <Console id={id} onCommitted={info.reload} />
        ) : tab === 'tables' ? (
          <Tables id={id} />
        ) : tab === 'history' ? (
          <History id={id} />
        ) : tab === 'branches' ? (
          <Branches id={id} branch={branchName(db.branch)} repository={db.repository} onMerged={info.reload} />
        ) : tab === 'settings' ? (
          <SettingsTab id={id} />
        ) : (
          <p>
            No such view. <Link to={`${base}/console`}>Open the SQL console.</Link>
          </p>
        )}
      </section>
    </>
  );
}
