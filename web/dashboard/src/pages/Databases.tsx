import { useState, type FormEvent } from 'react';

import { api, branchName, when, type DatabaseInfo, type Me } from '../api';
import { Link, navigate } from '../router';
import { ErrorNote, SecretOnce, Spinner, useAction, useLoad } from '../ui';

interface Repository {
  repository: string;
  installation: number;
  private: boolean;
}

function AddDatabase({ me, onAdded }: { me: Me; onAdded: () => void }) {
  const repositories = useLoad(
    () => api<{ repositories: Repository[] }>('GET', '/v1/repositories').then((r) => r.repositories),
    [],
  );
  const [repository, setRepository] = useState('');
  const [branch, setBranch] = useState('main');
  const [create, setCreate] = useState(true);
  const [encryption, setEncryption] = useState<'kms' | 'passphrase' | 'none'>(me.kms ? 'kms' : 'passphrase');
  const [passphrase, setPassphrase] = useState('');
  const [added, setAdded] = useState<DatabaseInfo | null>(null);
  const action = useAction();

  if (repositories.loading && !repositories.data) return <Spinner />;
  const list = repositories.data ?? [];
  const chosen = repository || list[0]?.repository || '';

  if (added?.recovery_key) {
    return (
      <SecretOnce title="The database's recovery key" secret={added.recovery_key}>
        <p>
          Keep it somewhere safe, away from the repository. It opens the database if the
          {added.encryption === 'passphrase' ? ' passphrase is lost' : ' service can no longer unwrap its key'}.
          It is not shown again.
        </p>
        <button type="button" className="button button-primary" onClick={() => navigate(`/databases/${added.id}`)}>
          I have kept it: open the database
        </button>
      </SecretOnce>
    );
  }

  if (list.length === 0) {
    return (
      <div className="note">
        <p>
          <strong>Install the OctoPage GitHub App on a repository first.</strong> A database lives in
          one of your repositories; the App lets the service reach it, and nothing else.
        </p>
        {me.install_url ? (
          <a className="button button-primary" href={me.install_url}>
            Install the GitHub App
          </a>
        ) : null}
        <button type="button" className="button" onClick={repositories.reload}>
          I have installed it
        </button>
      </div>
    );
  }

  const submit = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      const body: Record<string, unknown> = { repository: chosen, branch: branch.trim() || 'main', create };
      if (create) body.encryption = encryption;
      if (encryption === 'passphrase' || !create) {
        if (passphrase) body.passphrase = passphrase;
      }
      const info = await api<DatabaseInfo>('POST', '/v1/databases', body);
      onAdded();
      if (info.recovery_key) setAdded(info);
      else navigate(`/databases/${info.id}`);
    });
  };

  return (
    <form className="card form" onSubmit={submit} aria-labelledby="add-title">
      <h2 id="add-title">Add a database</h2>
      <div className="row">
        <label>
          Repository
          <select value={chosen} onChange={(e) => setRepository(e.target.value)}>
            {list.map((r) => (
              <option key={r.repository} value={r.repository}>
                {r.repository}
                {r.private ? '' : ' (public)'}
              </option>
            ))}
          </select>
        </label>
        <label>
          Branch
          <input value={branch} onChange={(e) => setBranch(e.target.value)} required pattern="[A-Za-z0-9._\/\-]+" />
        </label>
      </div>
      <fieldset>
        <legend>The branch</legend>
        <label className="choice">
          <input type="radio" checked={create} onChange={() => setCreate(true)} /> Create a new database on it
        </label>
        <label className="choice">
          <input type="radio" checked={!create} onChange={() => setCreate(false)} /> It holds a database already
        </label>
      </fieldset>
      {create ? (
        <fieldset>
          <legend>Encryption</legend>
          {me.kms ? (
            <label className="choice">
              <input type="radio" checked={encryption === 'kms'} onChange={() => setEncryption('kms')} /> Encrypted, key
              held by the service <span className="muted">(recommended)</span>
            </label>
          ) : null}
          <label className="choice">
            <input type="radio" checked={encryption === 'passphrase'} onChange={() => setEncryption('passphrase')} />{' '}
            Encrypted with my passphrase <span className="muted">(unlock it after each restart of the service)</span>
          </label>
          <label className="choice">
            <input type="radio" checked={encryption === 'none'} onChange={() => setEncryption('none')} /> Not encrypted{' '}
            <span className="muted">(anyone who can read the repository can read the data)</span>
          </label>
        </fieldset>
      ) : null}
      {(create && encryption === 'passphrase') || !create ? (
        <label>
          Passphrase {create ? '(at least 12 characters)' : '(if the database is encrypted with one)'}
          <input
            type="password"
            value={passphrase}
            onChange={(e) => setPassphrase(e.target.value)}
            minLength={create ? 12 : undefined}
            required={create}
            autoComplete="new-password"
          />
        </label>
      ) : null}
      <ErrorNote error={action.error} />
      <div className="actions">
        <button className="button button-primary" disabled={action.busy}>
          {action.busy ? 'Adding…' : create ? 'Create database' : 'Add database'}
        </button>
      </div>
    </form>
  );
}

export function Databases({ me }: { me: Me }) {
  const databases = useLoad(
    () => api<{ databases: DatabaseInfo[] }>('GET', '/v1/databases').then((r) => r.databases),
    [],
  );
  const [adding, setAdding] = useState(false);

  return (
    <>
      <div className="page-head">
        <h1>Databases</h1>
        {!adding && databases.data?.length ? (
          <button type="button" className="button button-primary" onClick={() => setAdding(true)}>
            Add a database
          </button>
        ) : null}
      </div>
      <ErrorNote error={databases.error} />
      {databases.loading && !databases.data ? <Spinner /> : null}
      {databases.data?.length ? (
        <div className="table-wrap">
          <table className="list">
            <thead>
              <tr>
                <th scope="col">Repository</th>
                <th scope="col">Branch</th>
                <th scope="col">Encryption</th>
                <th scope="col">Added</th>
              </tr>
            </thead>
            <tbody>
              {databases.data.map((d) => (
                <tr key={d.id}>
                  <td>
                    <Link to={`/databases/${d.id}`}>{d.repository}</Link>
                  </td>
                  <td>{branchName(d.branch)}</td>
                  <td>{d.encryption === 'none' ? 'none' : d.encryption === 'kms' ? 'service key' : 'passphrase'}</td>
                  <td>{when(d.created)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : null}
      {adding || databases.data?.length === 0 ? <AddDatabase me={me} onAdded={databases.reload} /> : null}
    </>
  );
}
