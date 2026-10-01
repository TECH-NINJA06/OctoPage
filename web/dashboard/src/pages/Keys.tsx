import { useState, type FormEvent } from 'react';

import { ago, api, when, type Key } from '../api';
import { ErrorNote, SecretOnce, Spinner, useAction, useLoad } from '../ui';

export function Keys() {
  const keys = useLoad(() => api<{ keys: Key[] }>('GET', '/v1/keys').then((r) => r.keys), []);
  const [name, setName] = useState('');
  const [created, setCreated] = useState<{ name: string; key: string } | null>(null);
  const action = useAction();

  const create = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      const answer = await api<{ key: string; name: string }>('POST', '/v1/keys', { name: name.trim() });
      setCreated(answer);
      setName('');
      keys.reload();
    });
  };
  const revoke = (key: Key) =>
    action.run(async () => {
      if (!confirm(`Revoke ${key.name}? Applications using it stop working at once.`)) return;
      await api('DELETE', `/v1/keys/${encodeURIComponent(key.id)}`);
      keys.reload();
    });

  return (
    <>
      <div className="page-head">
        <h1>API keys</h1>
      </div>
      <p className="muted">
        Applications use a key to call the API (<code>Authorization: Bearer opk_…</code>), with the SDKs for
        TypeScript and Python or plain HTTP. A key can do what you can: keep it secret.
      </p>
      {created ? (
        <SecretOnce title={`Your new key "${created.name}"`} secret={created.key}>
          <p>Copy it now: it is not shown again.</p>
          <button type="button" className="button" onClick={() => setCreated(null)}>
            Done
          </button>
        </SecretOnce>
      ) : null}
      <ErrorNote error={action.error ?? keys.error} />
      {keys.loading && !keys.data ? <Spinner /> : null}
      {keys.data?.length ? (
        <div className="table-wrap">
          <table className="list">
            <thead>
              <tr>
                <th scope="col">Name</th>
                <th scope="col">Key</th>
                <th scope="col">Created</th>
                <th scope="col">Last used</th>
                <th scope="col">
                  <span className="visually-hidden">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {keys.data.map((k) => (
                <tr key={k.id}>
                  <td>{k.name}</td>
                  <td>
                    <code>{k.prefix}…</code>
                  </td>
                  <td>{when(k.created)}</td>
                  <td>{k.last_used ? ago(k.last_used) : 'never'}</td>
                  <td>
                    <div className="row-actions">
                    <button type="button" className="button button-small button-danger" onClick={() => revoke(k)} disabled={action.busy}>
                      Revoke
                    </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : keys.data ? (
        <p className="muted">No keys yet.</p>
      ) : null}
      <form className="inline-form" onSubmit={create}>
        <label>
          New key named
          <input value={name} onChange={(e) => setName(e.target.value)} required maxLength={80} placeholder="production app" />
        </label>
        <button className="button button-primary" disabled={action.busy}>
          Create key
        </button>
      </form>
    </>
  );
}
