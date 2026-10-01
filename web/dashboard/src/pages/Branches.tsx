import { useState, type FormEvent } from 'react';

import { api, shortSha, type DatabaseInfo } from '../api';
import { Link, navigate } from '../router';
import { ErrorNote, Spinner, useAction, useLoad } from '../ui';

interface Branch {
  name: string;
  head: string;
}

export function Branches({
  id,
  branch,
  repository,
  onMerged,
}: {
  id: string;
  branch: string;
  repository: string;
  onMerged: () => void;
}) {
  const path = `/v1/databases/${encodeURIComponent(id)}`;
  const branches = useLoad(() => api<{ branches: Branch[] }>('GET', `${path}/branches`).then((r) => r.branches), [id]);
  const databases = useLoad(() => api<{ databases: DatabaseInfo[] }>('GET', '/v1/databases').then((r) => r.databases), []);
  const [name, setName] = useState('');
  const [notice, setNotice] = useState('');
  const action = useAction();

  const create = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      await api('POST', `${path}/branches`, { name: name.trim() });
      setNotice(`Branch ${name.trim()} created from ${branch}.`);
      setName('');
      branches.reload();
    });
  };
  const merge = (from: string) =>
    action.run(async () => {
      const report = await api<{ merged: unknown[]; skipped: number }>('POST', `${path}/merge`, { from });
      setNotice(
        report.merged.length
          ? `Merged ${report.merged.length} ${report.merged.length === 1 ? 'commit' : 'commits'} from ${from}.`
          : `Nothing to merge from ${from}.`,
      );
      onMerged();
      branches.reload();
    });
  const drop = (target: string) =>
    action.run(async () => {
      if (!confirm(`Delete branch ${target}? Changes on it that were not merged are lost.`)) return;
      await api('DELETE', `${path}/branches/${encodeURIComponent(target)}`);
      setNotice(`Branch ${target} deleted.`);
      branches.reload();
    });
  const open = (target: string) =>
    action.run(async () => {
      const info = await api<DatabaseInfo>('POST', '/v1/databases', { repository, branch: target });
      databases.reload();
      navigate(`/databases/${info.id}`);
    });

  const served = new Map(
    (databases.data ?? [])
      .filter((d) => d.repository.toLowerCase() === repository.toLowerCase())
      .map((d) => [d.branch.replace(/^refs\/heads\//, ''), d.id]),
  );

  return (
    <div className="branches">
      <p className="muted small">
        A branch is a copy of the database that changes on its own; merging replays its statements onto{' '}
        <strong>{branch}</strong>.
      </p>
      {notice ? (
        <p className="note" role="status">
          {notice}
        </p>
      ) : null}
      <ErrorNote error={action.error ?? branches.error} />
      {branches.loading && !branches.data ? <Spinner /> : null}
      <div className="table-wrap">
        <table className="list">
          <thead>
            <tr>
              <th scope="col">Branch</th>
              <th scope="col">Head</th>
              <th scope="col">
                <span className="visually-hidden">Actions</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {(branches.data ?? []).map((b) => (
              <tr key={b.name}>
                <td>
                  {b.name}
                  {b.name === branch ? <span className="badge">this one</span> : null}
                </td>
                <td>
                  <code>{shortSha(b.head)}</code>
                </td>
                <td>
                  {b.name === branch ? null : (
                    <div className="row-actions">
                    <>
                      {served.has(b.name) ? (
                        <Link className="button button-small" to={`/databases/${served.get(b.name)}`}>
                          Open
                        </Link>
                      ) : (
                        <button type="button" className="button button-small" onClick={() => open(b.name)} disabled={action.busy}>
                          Open
                        </button>
                      )}
                      <button type="button" className="button button-small" onClick={() => merge(b.name)} disabled={action.busy}>
                        Merge into {branch}
                      </button>
                      <button type="button" className="button button-small button-danger" onClick={() => drop(b.name)} disabled={action.busy}>
                        Delete
                      </button>
                    </>
                    </div>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <form className="inline-form" onSubmit={create}>
        <label>
          New branch from {branch}
          <input value={name} onChange={(e) => setName(e.target.value)} required pattern="[A-Za-z0-9._\-]+" placeholder="feature" />
        </label>
        <button className="button" disabled={action.busy}>
          Create branch
        </button>
      </form>
    </div>
  );
}
