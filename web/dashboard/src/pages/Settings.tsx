import { useEffect, useState, type FormEvent } from 'react';

import { ago, api, bytes, count, type Settings, type Stats } from '../api';
import { Meter, StatTile, type Level } from '../charts';
import { navigate } from '../router';
import { ErrorNote, Spinner, useAction, useLoad } from '../ui';

const MB = 1024 * 1024;

function days(n: number | null): string {
  if (n === null) return 'not growing';
  if (n > 3650) return 'over 10 years';
  if (n > 730) return `${Math.round(n / 365)} years`;
  return `${Math.max(0, Math.round(n))} days`;
}

function SizeReport({ stats, settings }: { stats: Stats; settings: Settings }) {
  const repository = stats.repository_bytes ?? stats.live_bytes;
  const repositoryLevel: Level =
    repository >= stats.repository_budget ? 'critical' : stats.warn ? 'warning' : 'ok';
  const liveLevel: Level =
    stats.live_bytes >= stats.live_limit ? 'critical' : stats.live_bytes >= stats.live_limit * 0.9 ? 'warning' : 'ok';
  return (
    <section aria-labelledby="size-title" className="card">
      <h2 id="size-title">Size</h2>
      <div className="tiles">
        <StatTile label="Days until the budget" value={days(stats.days_left)} detail={`at ${bytes(stats.bytes_per_day)} a day`} />
        <StatTile label="Commits in 30 days" value={count(stats.commits_30d)} />
        <StatTile label="Pages" value={count(stats.pages)} detail={`${stats.page_size / 1024} KB each`} />
      </div>
      <Meter
        label="Repository"
        value={repository}
        limit={stats.repository_budget}
        format={bytes}
        level={repositoryLevel}
        note={
          repositoryLevel === 'ok'
            ? stats.repository_bytes === null
              ? 'GitHub did not report the size; this is the live data.'
              : undefined
            : `at this rate the repository reaches its budget in ${days(stats.days_left)}. Compaction (a rollover) or a larger budget helps.`
        }
      />
      <Meter
        label="Live data"
        value={stats.live_bytes}
        limit={stats.live_limit}
        format={bytes}
        level={liveLevel}
        note={liveLevel === 'ok' ? undefined : 'writes that would grow the data past the limit fail.'}
      />
      <p className="muted small">
        Measured {ago(stats.measured)}; GitHub updates repository sizes with some delay. Warnings start{' '}
        {settings.warn_days} days before the budget.
      </p>
    </section>
  );
}

function RetentionField({ value, onChange }: { value: string; onChange: (value: string) => void }) {
  const [kind, number] = value.split(/\s+/);
  const set = (k: string, n: string) => onChange(k === 'keep_all' ? 'keep_all' : `${k} ${n || '30'}`);
  return (
    <fieldset>
      <legend>History to keep when the repository is compacted</legend>
      <div className="row">
        <label>
          Keep
          <select value={kind} onChange={(e) => set(e.target.value, number)}>
            <option value="keep_all">every commit</option>
            <option value="keep_days">commits of the last N days</option>
            <option value="keep_count">the last N commits</option>
          </select>
        </label>
        {kind !== 'keep_all' ? (
          <label>
            N
            <input type="number" min={1} value={number ?? ''} onChange={(e) => set(kind, e.target.value)} required />
          </label>
        ) : null}
      </div>
    </fieldset>
  );
}

function SettingsForm({ id, settings, onSaved }: { id: string; settings: Settings; onSaved: () => void }) {
  const [draft, setDraft] = useState(settings);
  const [saved, setSaved] = useState(false);
  const action = useAction();
  useEffect(() => setDraft(settings), [settings]);
  const mb = (field: 'live_limit' | 'repository_budget') => ({
    value: Math.round(draft[field] / MB),
    onChange: (e: { target: { value: string } }) => setDraft({ ...draft, [field]: Number(e.target.value) * MB }),
  });
  const submit = (event: FormEvent) => {
    event.preventDefault();
    setSaved(false);
    action.run(async () => {
      await api('PUT', `/v1/databases/${encodeURIComponent(id)}/settings`, draft);
      setSaved(true);
      onSaved();
    });
  };
  return (
    <form className="card form" onSubmit={submit} aria-labelledby="settings-title">
      <h2 id="settings-title">Settings</h2>
      <RetentionField value={draft.retention} onChange={(retention) => setDraft({ ...draft, retention })} />
      <div className="row">
        <label>
          Live data limit (MB)
          <input type="number" min={1} {...mb('live_limit')} />
        </label>
        <label>
          Repository budget (MB)
          <input type="number" min={1} {...mb('repository_budget')} />
        </label>
        <label>
          Warn this many days ahead
          <input type="number" min={1} value={draft.warn_days} onChange={(e) => setDraft({ ...draft, warn_days: Number(e.target.value) })} />
        </label>
      </div>
      <ErrorNote error={action.error} />
      {saved ? (
        <p className="note" role="status">
          Saved, as a commit beside the data.
        </p>
      ) : null}
      <div className="actions">
        <button className="button button-primary" disabled={action.busy}>
          Save settings
        </button>
      </div>
    </form>
  );
}

function Maintenance({ id }: { id: string }) {
  const [source, setSource] = useState('');
  const [installed, setInstalled] = useState<string | null>(null);
  const action = useAction();
  const submit = (event: FormEvent) => {
    event.preventDefault();
    action.run(async () => {
      const answer = await api<{ path: string }>('POST', `/v1/databases/${encodeURIComponent(id)}/maintenance`, {
        cli_source: source.trim() || undefined,
      });
      setInstalled(answer.path);
    });
  };
  return (
    <form className="card form" onSubmit={submit} aria-labelledby="maintenance-title">
      <h2 id="maintenance-title">Maintenance</h2>
      <p>
        A GitHub Actions workflow, committed to the repository, that checks every page each week,
        cleans up, and warns (with an issue) before the repository reaches its budget.
      </p>
      <label>
        Build the CLI from <span className="muted">(leave empty for the service's default)</span>
        <input value={source} onChange={(e) => setSource(e.target.value)} placeholder="https://github.com/…/octopage" />
      </label>
      <ErrorNote error={action.error} />
      {installed ? (
        <p className="note" role="status">
          Installed at <code>{installed}</code>. It runs weekly; start it by hand from the repository's Actions tab.
        </p>
      ) : null}
      <div className="actions">
        <button className="button" disabled={action.busy}>
          Install the maintenance workflow
        </button>
      </div>
    </form>
  );
}

function Remove({ id }: { id: string }) {
  const action = useAction();
  const remove = () =>
    action.run(async () => {
      if (!confirm('Stop serving this database? Its data stays in the repository; you can add it again.')) return;
      await api('DELETE', `/v1/databases/${encodeURIComponent(id)}`);
      navigate('/');
    });
  return (
    <section className="card" aria-labelledby="remove-title">
      <h2 id="remove-title">Remove</h2>
      <p>The service stops serving the database. The repository and its data are not touched.</p>
      <ErrorNote error={action.error} />
      <button type="button" className="button button-danger" onClick={remove} disabled={action.busy}>
        Remove from the service
      </button>
    </section>
  );
}

export function SettingsTab({ id }: { id: string }) {
  const path = `/v1/databases/${encodeURIComponent(id)}`;
  const settings = useLoad(() => api<Settings>('GET', `${path}/settings`), [id]);
  const stats = useLoad(() => api<Stats>('GET', `${path}/stats`), [id]);
  const refresh = () => {
    settings.reload();
    stats.reload();
  };
  return (
    <div className="settings">
      <ErrorNote error={stats.error} />
      {stats.data && settings.data ? (
        <div className={stats.loading ? 'refreshing' : undefined}>
          <SizeReport stats={stats.data} settings={settings.data} />
        </div>
      ) : stats.loading ? (
        <Spinner label="Measuring the database (this reads a month of history)…" />
      ) : null}
      <ErrorNote error={settings.error} />
      {settings.data ? <SettingsForm id={id} settings={settings.data} onSaved={refresh} /> : null}
      <Maintenance id={id} />
      <Remove id={id} />
    </div>
  );
}
