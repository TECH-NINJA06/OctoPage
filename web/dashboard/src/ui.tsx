// Shared pieces: loading data, errors, buttons, results, the SQL editor.

import { sql, SQLite } from '@codemirror/lang-sql';
import { EditorView, keymap } from '@codemirror/view';
import { basicSetup } from 'codemirror';
import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';

import { ApiError, display, type Result, type SqlValue } from './api';
import { Link } from './router';

/** Load something; reload on demand or when `deps` change. Keeps the last data while reloading. */
export function useLoad<T>(load: () => Promise<T>, deps: unknown[]) {
  const [state, setState] = useState<{ data?: T; error?: unknown; loading: boolean }>({
    loading: true,
  });
  const generation = useRef(0);
  const run = useCallback(load, deps);
  const reload = useCallback(() => {
    const mine = ++generation.current;
    setState((s) => ({ ...s, loading: true }));
    run().then(
      (data) => mine === generation.current && setState({ data, loading: false }),
      (error) => mine === generation.current && setState((s) => ({ ...s, error, loading: false })),
    );
  }, [run]);
  useEffect(reload, [reload]);
  return { ...state, reload };
}

export function message(error: unknown): string {
  if (error instanceof ApiError) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

export function ErrorNote({ error }: { error: unknown }) {
  if (!error) return null;
  return (
    <p className="note note-error" role="alert">
      {message(error)}
    </p>
  );
}

export function Spinner({ label = 'Loading…' }: { label?: string }) {
  return (
    <p className="muted" role="status">
      {label}
    </p>
  );
}

/** Run an action from a button: busy while it runs, its error shown beside it. */
export function useAction() {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const run = useCallback(async (action: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await action();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  }, []);
  return { busy, error, run, setError };
}

export function CopyButton({ text, label = 'Copy' }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      className="button button-small"
      onClick={async () => {
        await navigator.clipboard.writeText(text);
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      }}
    >
      {copied ? 'Copied' : label}
    </button>
  );
}

/** A secret shown once, with a copy button and a warning. */
export function SecretOnce({ title, secret, children }: { title: string; secret: string; children?: ReactNode }) {
  return (
    <div className="note note-secret" role="status">
      <strong>{title}</strong>
      <div className="secret">
        <code data-testid="secret">{secret}</code>
        <CopyButton text={secret} />
      </div>
      {children}
    </div>
  );
}

function Cell({ value }: { value: SqlValue }) {
  const kind =
    value === null ? 'null' : value instanceof Uint8Array ? 'blob' : typeof value === 'string' ? 'text' : 'number';
  return <td className={`cell-${kind}`}>{display(value)}</td>;
}

/** Rows as a table, or what a statement did. */
export function ResultTable({ result, empty = 'No rows.' }: { result: Result; empty?: string }) {
  if (result.columns.length === 0) {
    return (
      <p className="muted">
        {result.changed} {result.changed === 1 ? 'row' : 'rows'} changed
      </p>
    );
  }
  return (
    <div className="table-wrap">
      <table className="data">
        <thead>
          <tr>
            {result.columns.map((c, i) => (
              <th key={i} scope="col">
                {c}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {result.rows.length === 0 ? (
            <tr>
              <td colSpan={result.columns.length} className="muted">
                {empty}
              </td>
            </tr>
          ) : (
            result.rows.map((row, r) => (
              <tr key={r}>
                {row.map((value, c) => (
                  <Cell key={c} value={value} />
                ))}
              </tr>
            ))
          )}
        </tbody>
      </table>
    </div>
  );
}

/** A SQL editor (CodeMirror). Ctrl-Enter (or Cmd-Enter) runs. */
export function SqlEditor({
  value,
  onChange,
  onRun,
  label,
}: {
  value: string;
  onChange: (value: string) => void;
  onRun: () => void;
  label: string;
}) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const handlers = useRef({ onChange, onRun });
  handlers.current = { onChange, onRun };

  useEffect(() => {
    const editor = new EditorView({
      doc: value,
      parent: host.current!,
      extensions: [
        keymap.of([
          {
            key: 'Mod-Enter',
            run: () => {
              handlers.current.onRun();
              return true;
            },
          },
        ]),
        basicSetup,
        sql({ dialect: SQLite }),
        EditorView.lineWrapping,
        EditorView.contentAttributes.of({ 'aria-label': label }),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) handlers.current.onChange(update.state.doc.toString());
        }),
      ],
    });
    view.current = editor;
    return () => editor.destroy();
    // The editor owns its text after creation; `value` changes from outside are applied below.
  }, []);

  useEffect(() => {
    const editor = view.current;
    if (editor && editor.state.doc.toString() !== value) {
      editor.dispatch({ changes: { from: 0, to: editor.state.doc.length, insert: value } });
    }
  }, [value]);

  return <div className="editor" ref={host} data-testid="sql-editor" />;
}

/** Tabs as links, so each has its own address. */
export function Tabs({ tabs, current }: { tabs: { id: string; label: string; href: string }[]; current: string }) {
  return (
    <nav className="tabs" aria-label="Database views">
      {tabs.map((t) => (
        <TabLink key={t.id} href={t.href} selected={t.id === current}>
          {t.label}
        </TabLink>
      ))}
    </nav>
  );
}

function TabLink({ href, selected, children }: { href: string; selected: boolean; children: ReactNode }) {
  return (
    <Link to={href} className={selected ? 'tab tab-selected' : 'tab'} aria-current={selected ? 'page' : undefined}>
      {children}
    </Link>
  );
}
