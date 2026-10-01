import { useState } from 'react';

import { ago, api, decode, display, shortSha, when, type Commit } from '../api';
import { Link } from '../router';
import { ErrorNote, Spinner, useLoad } from '../ui';

/** A statement with its parameters filled in, for reading. */
function statementText(sql: string, params: unknown[]): string {
  if (params.length === 0) return sql;
  const shown = params.map((p) => {
    const v = decode(p);
    return typeof v === 'string' ? `'${v}'` : display(v);
  });
  return `${sql}   -- ${shown.join(', ')}`;
}

export function History({ id }: { id: string }) {
  const [limit, setLimit] = useState(25);
  const log = useLoad(
    () => api<{ commits: Commit[] }>('GET', `/v1/databases/${encodeURIComponent(id)}/log?limit=${limit}`).then((r) => r.commits),
    [id, limit],
  );
  const base = `/databases/${encodeURIComponent(id)}`;

  return (
    <div className="history">
      <p className="muted small">
        Every change is a git commit in the repository. Each lists the statements it ran; open one to see
        the tables as they were right after it.
      </p>
      <ErrorNote error={log.error} />
      {log.loading && !log.data ? <Spinner /> : null}
      <ol className="commits" data-testid="commits">
        {(log.data ?? []).map((c) => (
          <li key={c.commit} className="commit">
            <div className="commit-head">
              <code title={c.commit}>{shortSha(c.commit)}</code>
              <span className="muted" title={when(c.time)}>
                {ago(c.time)}
              </span>
              <span className="grow" />
              <Link to={`${base}/tables?asof=${c.commit}`} className="button button-small">
                Browse as of this commit
              </Link>
            </div>
            {c.statements.length ? (
              <pre className="statements">{c.statements.map((s) => statementText(s.sql, s.params)).join(';\n')}</pre>
            ) : (
              <p className="muted small">{c.message.split('\n')[0] || 'No statements (a change beside the data).'}</p>
            )}
          </li>
        ))}
      </ol>
      {log.data && log.data.length >= limit ? (
        <button type="button" className="button" onClick={() => setLimit(limit * 2)} disabled={log.loading}>
          Show older commits
        </button>
      ) : null}
    </div>
  );
}
