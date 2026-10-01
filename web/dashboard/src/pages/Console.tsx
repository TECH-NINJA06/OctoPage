import { useState } from 'react';

import { api, ApiError, result, shortSha, type Result } from '../api';
import { ErrorNote, ResultTable, SqlEditor, useAction } from '../ui';

interface Run {
  results: Result[];
  commit: string | null;
  ms: number;
  statements: number;
}

const START = `SELECT name, type FROM sqlite_schema ORDER BY name;`;

const DRAFT_KEY = (id: string) => `octopage.console.${id}`;

function draft(id: string): string {
  try {
    return localStorage.getItem(DRAFT_KEY(id)) ?? START;
  } catch {
    return START;
  }
}

export function Console({ id, onCommitted }: { id: string; onCommitted: () => void }) {
  const [text, setText] = useState(() => draft(id));
  const [run, setRun] = useState<Run | null>(null);
  const action = useAction();
  const path = `/v1/databases/${encodeURIComponent(id)}`;

  const change = (value: string) => {
    setText(value);
    try {
      localStorage.setItem(DRAFT_KEY(id), value);
    } catch {
      // No storage: the draft is simply not kept.
    }
  };

  const execute = () => {
    const sql = text.trim();
    if (!sql || action.busy) return;
    action.run(async () => {
      const started = performance.now();
      let outcome: Run;
      setRun(null);
      try {
        const one = result(await api('POST', `${path}/execute`, { sql }));
        outcome = { results: [one], commit: one.commit, ms: 0, statements: 1 };
      } catch (error) {
        // Several statements: one transaction, as a batch.
        if (!(error instanceof ApiError && error.code === 'multiple_statements')) throw error;
        const batch = await api<{ results: unknown[]; commit: string | null }>('POST', `${path}/batch`, { sql });
        const results = batch.results.map(result);
        outcome = { results, commit: batch.commit, ms: 0, statements: results.length };
      }
      outcome.ms = Math.round(performance.now() - started);
      setRun(outcome);
      if (outcome.commit) onCommitted();
    });
  };

  const last = run?.results[run.results.length - 1];
  const shown = run ? [...run.results].reverse().find((r) => r.columns.length > 0) ?? last : undefined;

  return (
    <div className="console">
      <SqlEditor value={text} onChange={change} onRun={execute} label="SQL" />
      <div className="actions">
        <button type="button" className="button button-primary" onClick={execute} disabled={action.busy} data-testid="run">
          {action.busy ? 'Running…' : 'Run'}
        </button>
        <span className="muted small">
          Ctrl-Enter runs. Several statements run as one transaction. End a query with{' '}
          <code>AS OF '2026-09-01'</code> (or a commit id) to read the past.
        </span>
      </div>
      <ErrorNote error={action.error} />
      {run && shown ? (
        <div className="run" data-testid="result">
          <p className="muted small" data-testid="run-summary">
            {run.statements > 1 ? `${run.statements} statements · ` : ''}
            {shown.columns.length ? `${shown.rows.length} ${shown.rows.length === 1 ? 'row' : 'rows'} · ` : ''}
            {run.ms} ms
            {run.commit ? (
              <>
                {' '}
                · committed <code>{shortSha(run.commit)}</code>
              </>
            ) : null}
          </p>
          <ResultTable result={shown} />
        </div>
      ) : null}
    </div>
  );
}
