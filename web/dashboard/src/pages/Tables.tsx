import { useEffect, useState, type FormEvent } from 'react';

import { api, ident, literal, result, shortSha, type Result } from '../api';
import { navigate, useLocation } from '../router';
import { ErrorNote, ResultTable, Spinner, useLoad } from '../ui';

const PAGE = 100;

/** Browse tables and views, now or as of a commit or time (`?asof=`). */
export function Tables({ id }: { id: string }) {
  const { query } = useLocation();
  const asOf = query.get('asof') ?? '';
  const table = query.get('table') ?? '';
  const [offset, setOffset] = useState(0);
  const [asOfInput, setAsOfInput] = useState(asOf);
  const path = `/v1/databases/${encodeURIComponent(id)}`;
  const suffix = asOf ? ` AS OF ${literal(asOf)}` : '';
  const run = async (sql: string): Promise<Result> =>
    result(await api('POST', `${path}/query`, { sql: sql + suffix }));

  useEffect(() => setAsOfInput(asOf), [asOf]);
  useEffect(() => setOffset(0), [table, asOf]);

  const tables = useLoad(
    () =>
      run(
        "SELECT name, type FROM sqlite_schema WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name",
      ),
    [id, asOf],
  );
  const names = (tables.data?.rows ?? []).map((r) => String(r[0]));
  const current = names.includes(table) ? table : names[0] ?? '';
  const rows = useLoad(
    () => (current ? run(`SELECT * FROM ${ident(current)} LIMIT ${PAGE + 1} OFFSET ${offset}`) : Promise.resolve(null)),
    [id, asOf, current, offset],
  );
  const total = useLoad(
    () => (current ? run(`SELECT count(*) FROM ${ident(current)}`) : Promise.resolve(null)),
    [id, asOf, current],
  );

  const go = (changes: Record<string, string>) => {
    const next = new URLSearchParams(query);
    for (const [k, v] of Object.entries(changes)) {
      if (v) next.set(k, v);
      else next.delete(k);
    }
    const search = next.toString();
    navigate(`${location.pathname}${search ? `?${search}` : ''}`);
  };
  const applyAsOf = (event: FormEvent) => {
    event.preventDefault();
    go({ asof: asOfInput.trim() });
  };

  const page = rows.data ? { ...rows.data, rows: rows.data.rows.slice(0, PAGE) } : null;
  const more = (rows.data?.rows.length ?? 0) > PAGE;
  const count = total.data?.rows[0]?.[0];

  return (
    <div className="browser">
      <form className="filters" onSubmit={applyAsOf} role="search">
        <label>
          As of
          <input
            value={asOfInput}
            onChange={(e) => setAsOfInput(e.target.value)}
            placeholder="now (or a commit id, or 2026-09-01 14:30)"
            size={34}
          />
        </label>
        <button className="button">Show</button>
        {asOf ? (
          <button type="button" className="button" onClick={() => go({ asof: '' })}>
            Back to now
          </button>
        ) : null}
        {asOf ? (
          <span className="badge" data-testid="as-of">
            Reading as of {/^[0-9a-f]{4,40}$/.test(asOf) ? shortSha(asOf) : asOf}
          </span>
        ) : null}
      </form>
      <ErrorNote error={tables.error} />
      {tables.loading && !tables.data ? <Spinner /> : null}
      {tables.data && names.length === 0 ? <p className="muted">No tables{asOf ? ' then' : ' yet'}.</p> : null}
      {names.length ? (
        <div className="split">
          <nav className="table-list" aria-label="Tables">
            {tables.data!.rows.map(([name, type]) => (
              <button
                key={String(name)}
                type="button"
                className={String(name) === current ? 'table-item table-item-selected' : 'table-item'}
                aria-current={String(name) === current ? 'true' : undefined}
                onClick={() => go({ table: String(name) })}
              >
                {String(name)}
                {type === 'view' ? <span className="muted small"> view</span> : null}
              </button>
            ))}
          </nav>
          <div className="table-rows">
            <div className="pager">
              <strong>{current}</strong>
              {typeof count === 'number' || typeof count === 'bigint' ? (
                <span className="muted">
                  {' '}
                  · {String(count)} rows{page && page.rows.length ? ` · showing ${offset + 1}–${offset + page.rows.length}` : ''}
                </span>
              ) : null}
              <span className="grow" />
              <button type="button" className="button button-small" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - PAGE))}>
                Previous
              </button>
              <button type="button" className="button button-small" disabled={!more} onClick={() => setOffset(offset + PAGE)}>
                Next
              </button>
            </div>
            <ErrorNote error={rows.error} />
            {page ? <ResultTable result={page} empty="The table is empty." /> : <Spinner />}
          </div>
        </div>
      ) : null}
    </div>
  );
}
