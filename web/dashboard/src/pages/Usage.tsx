import { useState } from 'react';

import { api, count, type UsageDay } from '../api';
import { ColumnChart, StatTile, type Column } from '../charts';
import { ErrorNote, Spinner, useLoad } from '../ui';

interface InstallationUsage {
  installation: number;
  account: string;
  days: UsageDay[];
}

const RANGES = [7, 30, 90];
const DAY = 86_400;

function dayLabel(day: number): string {
  return new Date(day * DAY * 1000).toLocaleDateString(undefined, { month: 'short', day: 'numeric', timeZone: 'UTC' });
}

/** Every day of the range, with the days nothing happened as zeros. */
function filled(days: UsageDay[], range: number): UsageDay[] {
  const today = Math.floor(Date.now() / 1000 / DAY);
  const byDay = new Map(days.map((d) => [d.day, d]));
  return Array.from({ length: range }, (_, i) => {
    const day = today - range + 1 + i;
    return byDay.get(day) ?? { day, requests: 0, github_requests: 0, commits: 0 };
  });
}

function Installation({ usage, range }: { usage: InstallationUsage; range: number }) {
  const [table, setTable] = useState(false);
  const days = filled(usage.days, range);
  const sum = (field: keyof UsageDay) => days.reduce((total, d) => total + d[field], 0);
  const columns: Column[] = days.map((d) => ({ key: String(d.day), label: dayLabel(d.day), value: d.requests }));
  return (
    <section className="card" aria-labelledby={`usage-${usage.installation}`}>
      <h2 id={`usage-${usage.installation}`}>{usage.account}</h2>
      <div className="tiles">
        <StatTile label="Requests" value={count(sum('requests'))} detail={`last ${range} days`} />
        <StatTile label="Requests to GitHub" value={count(sum('github_requests'))} detail="from this installation's budget" />
        <StatTile label="Commits" value={count(sum('commits'))} />
      </div>
      <div className="chart-head">
        <h3>Requests a day</h3>
        <button type="button" className="button button-small" onClick={() => setTable(!table)} aria-pressed={table}>
          {table ? 'Show chart' : 'Show table'}
        </button>
      </div>
      {table ? (
        <div className="table-wrap">
          <table className="numbers">
            <thead>
              <tr>
                <th scope="col">Day</th>
                <th scope="col">Requests</th>
                <th scope="col">Requests to GitHub</th>
                <th scope="col">Commits</th>
              </tr>
            </thead>
            <tbody>
              {[...days].reverse().map((d) => (
                <tr key={d.day}>
                  <td>{dayLabel(d.day)}</td>
                  <td>{d.requests}</td>
                  <td>{d.github_requests}</td>
                  <td>{d.commits}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <ColumnChart columns={columns} unit="requests" title={`Requests a day to ${usage.account}'s databases, last ${range} days`} />
      )}
    </section>
  );
}

export function Usage() {
  const [range, setRange] = useState(30);
  const usage = useLoad(
    () => api<{ installations: InstallationUsage[] }>('GET', `/v1/usage?days=${range}`).then((r) => r.installations),
    [range],
  );
  return (
    <>
      <div className="page-head">
        <h1>Usage</h1>
      </div>
      <div className="filters" role="group" aria-label="Time range">
        {RANGES.map((r) => (
          <button
            key={r}
            type="button"
            className={r === range ? 'button button-small button-selected' : 'button button-small'}
            aria-pressed={r === range}
            onClick={() => setRange(r)}
          >
            Last {r} days
          </button>
        ))}
      </div>
      <p className="muted small">Per installation of the GitHub App. Counts are written about once a minute.</p>
      <ErrorNote error={usage.error} />
      {usage.loading && !usage.data ? <Spinner /> : null}
      <div className={usage.loading ? 'refreshing' : undefined}>
        {usage.data?.length === 0 ? <p className="muted">No installations yet.</p> : null}
        {(usage.data ?? []).map((u) => (
          <Installation key={u.installation} usage={u} range={range} />
        ))}
      </div>
    </>
  );
}
