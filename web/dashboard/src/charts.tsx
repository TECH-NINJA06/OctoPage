// The dashboard's figures: stat tiles, meters (a value against its limit), and a column
// chart of one series over days, with a tooltip on hover and focus and a table view.

import { useEffect, useRef, useState, type ReactNode } from 'react';

export function StatTile({ label, value, detail }: { label: string; value: ReactNode; detail?: ReactNode }) {
  return (
    <div className="tile">
      <div className="tile-label">{label}</div>
      <div className="tile-value">{value}</div>
      {detail ? <div className="tile-detail">{detail}</div> : null}
    </div>
  );
}

export type Level = 'ok' | 'warning' | 'critical';

const LEVEL_TEXT: Record<Level, string> = { ok: '', warning: 'Warning', critical: 'Over the limit' };

/** A value against a limit. The fill carries the level; a warning always has its words too. */
export function Meter({
  label,
  value,
  limit,
  format,
  level,
  note,
}: {
  label: string;
  value: number;
  limit: number;
  format: (n: number) => string;
  level: Level;
  note?: string;
}) {
  const share = limit > 0 ? Math.min(value / limit, 1) : 0;
  return (
    <div className="meter-block">
      <div className="meter-head">
        <span className="meter-label">{label}</span>
        <span className="meter-value">
          {format(value)} <span className="muted">of {format(limit)}</span>
        </span>
      </div>
      <div
        className={`meter meter-${level}`}
        role="meter"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={limit}
        aria-valuenow={Math.min(value, limit)}
        aria-valuetext={`${format(value)} of ${format(limit)}`}
      >
        <div className="meter-fill" style={{ width: `${(share * 100).toFixed(2)}%` }} />
      </div>
      {level !== 'ok' || note ? (
        <p className={`meter-note ${level !== 'ok' ? `status-${level}` : 'muted'}`}>
          {level !== 'ok' ? (
            <>
              <span className="status-icon" aria-hidden="true">
                {level === 'critical' ? '⛔' : '⚠'}
              </span>
              <strong>{LEVEL_TEXT[level]}</strong>
              {note ? ': ' : ''}
            </>
          ) : null}
          {note}
        </p>
      ) : null}
    </div>
  );
}

/** Round numbers for the value axis (counts: whole ones), 0 and up to four steps above the largest. */
function ticks(max: number): number[] {
  if (max <= 0) return [0, 1];
  const rough = max / 4;
  const power = 10 ** Math.max(0, Math.floor(Math.log10(rough)));
  const step = [1, 2, 5, 10].map((m) => m * power).find((s) => s >= rough) ?? power * 10;
  const out = [];
  for (let v = 0; v < max + step; v += step) out.push(v);
  return out;
}

export interface Column {
  key: string;
  label: string;
  value: number;
}

const HEIGHT = 180;
const MARGIN = { top: 12, right: 8, bottom: 26, left: 44 };

/**
 * One series as columns (days, say). Columns are at most 24px wide with a 2px gap, their
 * tops rounded; each is a hover and focus target showing its value.
 */
export function ColumnChart({ columns, unit, title }: { columns: Column[]; unit: string; title: string }) {
  const box = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(600);
  const [active, setActive] = useState<number | null>(null);
  useEffect(() => {
    const element = box.current!;
    const observer = new ResizeObserver(([entry]) => setWidth(Math.max(240, entry.contentRect.width)));
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const max = Math.max(0, ...columns.map((c) => c.value));
  const scale = ticks(max);
  const top = scale[scale.length - 1];
  const plotWidth = width - MARGIN.left - MARGIN.right;
  const plotHeight = HEIGHT - MARGIN.top - MARGIN.bottom;
  const band = plotWidth / Math.max(columns.length, 1);
  const barWidth = Math.max(2, Math.min(24, band - 2));
  const y = (v: number) => MARGIN.top + plotHeight - (v / top) * plotHeight;
  const format = new Intl.NumberFormat();
  const labelled = new Set([0, Math.floor((columns.length - 1) / 2), columns.length - 1]);
  // Above its column, or beside it when the column is too tall to leave room.
  const tooltipAt = (i: number) => {
    const left = MARGIN.left + i * band;
    const top = y(columns[i].value) - 58;
    if (top >= 0) return { left: Math.min(width - 150, Math.max(0, left + band / 2 - 70)), top };
    return { left: left > 160 ? left - 152 : left + band + 8, top: MARGIN.top };
  };

  return (
    <div className="chart" ref={box}>
      <svg width={width} height={HEIGHT} role="img" aria-label={title}>
        {scale.map((v) => (
          <g key={v}>
            <line
              className={v === 0 ? 'axis-line' : 'grid-line'}
              x1={MARGIN.left}
              x2={width - MARGIN.right}
              y1={y(v)}
              y2={y(v)}
            />
            <text className="axis-text" x={MARGIN.left - 6} y={y(v)} dy="0.32em" textAnchor="end">
              {format.format(v)}
            </text>
          </g>
        ))}
        {columns.map((c, i) => {
          const x = MARGIN.left + i * band + (band - barWidth) / 2;
          const h = y(0) - y(c.value);
          const r = Math.min(4, barWidth / 2, h);
          const base = y(0);
          const path =
            h <= 0
              ? ''
              : `M${x},${base} V${base - h + r} Q${x},${base - h} ${x + r},${base - h} H${x + barWidth - r} ` +
                `Q${x + barWidth},${base - h} ${x + barWidth},${base - h + r} V${base} Z`;
          return (
            <g key={c.key}>
              {path ? <path className={i === active ? 'column column-active' : 'column'} d={path} /> : null}
              <rect
                className="hit"
                x={MARGIN.left + i * band}
                y={MARGIN.top}
                width={band}
                height={plotHeight}
                tabIndex={0}
                aria-label={`${c.label}: ${format.format(c.value)} ${unit}`}
                onPointerEnter={() => setActive(i)}
                onPointerLeave={() => setActive(null)}
                onFocus={() => setActive(i)}
                onBlur={() => setActive(null)}
              />
              {labelled.has(i) ? (
                <text
                  className="axis-text"
                  x={MARGIN.left + i * band + band / 2}
                  y={HEIGHT - 8}
                  textAnchor={i === 0 ? 'start' : i === columns.length - 1 ? 'end' : 'middle'}
                >
                  {c.label}
                </text>
              ) : null}
            </g>
          );
        })}
      </svg>
      {active !== null && columns[active] ? (
        <div className="tooltip" role="status" style={tooltipAt(active)}>
          <strong>
            {format.format(columns[active].value)} {unit}
          </strong>
          <span className="muted">{columns[active].label}</span>
        </div>
      ) : null}
    </div>
  );
}
