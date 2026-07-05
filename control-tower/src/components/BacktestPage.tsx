'use client';

import { useMemo, useState } from 'react';
import useSWR from 'swr';
import {
  LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, ReferenceLine,
} from 'recharts';
import { getBacktestRuns, getBacktestRun, runBacktest } from '@/lib/api';
import { DEMO_MODE } from '@/lib/demo';
import type { BacktestRunRequest, BacktestRunStatus, BacktestTrade } from '@/lib/types';

// ── Static option lists ───────────────────────────────────────────────────────

/** Short strategy names accepted by the harness (`configure_enables`). Momentum and
 *  Convergence are effectively excluded by the fidelity tiers (Tier B/C) but remain
 *  selectable so the exclusion is visible in the results. */
const STRATS: { value: string; label: string }[] = [
  { value: 'trendreversal', label: 'TrendReversal' },
  { value: 'gboost',        label: 'GBoost' },
  { value: 'basis',         label: 'Basis' },
  { value: 'maker',         label: 'Maker' },
  { value: 'timedecay',     label: 'Time Decay' },
  { value: 'arbitrage',     label: 'Arbitrage' },
  { value: 'momentum',      label: 'Momentum' },
  { value: 'convergence',   label: 'Convergence' },
];

const WINDOWS: { value: string; label: string }[] = [
  { value: '6h',     label: '6h' },
  { value: '24h',    label: '24h' },
  { value: '72h',    label: '72h' },
  { value: 'custom', label: 'Custom' },
];

// ── Formatting helpers ────────────────────────────────────────────────────────

function fmtNum(v: string | number | null | undefined, dp = 2): string {
  if (v === null || v === undefined) return '—';
  const n = typeof v === 'number' ? v : parseFloat(v);
  return Number.isFinite(n) ? n.toFixed(dp) : '—';
}

function fmtPct(v: number | null | undefined, dp = 2): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return '—';
  return `${v.toFixed(dp)}%`;
}

function fmtTs(iso: string): string {
  try {
    return new Date(iso).toLocaleString('en-US', {
      month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false,
    });
  } catch {
    return iso;
  }
}

function fmtMs(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return '—';
  return fmtTs(new Date(ms).toISOString());
}

function pnlClass(v: string | number | null | undefined): string {
  if (v === null || v === undefined) return 'text-gray-400';
  const n = typeof v === 'number' ? v : parseFloat(v);
  if (!Number.isFinite(n)) return 'text-gray-400';
  return n > 0 ? 'text-green-400' : n < 0 ? 'text-red-400' : 'text-gray-400';
}

const STATUS_META: Record<BacktestRunStatus, { label: string; cls: string; dot: string }> = {
  running: { label: 'Running', cls: 'text-amber-300 border-amber-500/30 bg-amber-500/10', dot: 'bg-amber-400 animate-pulse' },
  done:    { label: 'Done',    cls: 'text-green-300 border-green-500/30 bg-green-500/10', dot: 'bg-green-400' },
  failed:  { label: 'Failed',  cls: 'text-red-300 border-red-500/30 bg-red-500/10',       dot: 'bg-red-500' },
};

// ── Small shared bits ─────────────────────────────────────────────────────────

function StatusBadge({ status }: { status: BacktestRunStatus }) {
  const m = STATUS_META[status];
  return (
    <span className={`inline-flex items-center gap-1.5 text-[10px] font-mono rounded px-1.5 py-0.5 border ${m.cls}`}>
      <span className={`h-1.5 w-1.5 rounded-full ${m.dot}`} />
      {m.label}
    </span>
  );
}

function Field({ label, children, hint }: { label: string; children: React.ReactNode; hint?: string }) {
  return (
    <label className="flex flex-col gap-1">
      <span className="text-[11px] font-mono text-gray-500 uppercase tracking-wide">{label}</span>
      {children}
      {hint && <span className="text-[10px] font-mono text-gray-600">{hint}</span>}
    </label>
  );
}

// ── Run form ──────────────────────────────────────────────────────────────────

function RunForm({
  assets, busy, onRun,
}: {
  assets: string[];
  busy: boolean;
  onRun: (req: BacktestRunRequest) => Promise<void>;
}) {
  const [coin, setCoin] = useState<string>((assets[0] ?? 'btc').toUpperCase());
  const [win, setWin] = useState<string>('24h');
  const [customStart, setCustomStart] = useState<string>('');
  const [customEnd, setCustomEnd] = useState<string>('');
  const [interval, setIntervalStr] = useState<string>('1m');
  const [spread, setSpread] = useState<string>('0.02');
  const [depth, setDepth] = useState<string>('500');
  const [commission, setCommission] = useState<string>('0');
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [llmScore, setLlmScore] = useState<boolean>(false);
  const [submitting, setSubmitting] = useState<boolean>(false);
  const [error, setError] = useState<string | null>(null);

  const coinOptions = assets.length > 0 ? assets.map(a => a.toUpperCase()) : ['BTC', 'ETH', 'SOL'];

  function toggleStrat(v: string) {
    setSelected(prev => {
      const next = new Set(prev);
      if (next.has(v)) next.delete(v); else next.add(v);
      return next;
    });
  }

  function resolveWindow(): { start: string; end: string } | { error: string } {
    if (win === 'custom') {
      if (!customStart || !customEnd) return { error: 'Custom window needs both a start and an end.' };
      const s = new Date(customStart);
      const e = new Date(customEnd);
      if (Number.isNaN(s.getTime()) || Number.isNaN(e.getTime())) return { error: 'Invalid custom date/time.' };
      if (e.getTime() <= s.getTime()) return { error: 'End must be after start.' };
      return { start: s.toISOString(), end: e.toISOString() };
    }
    // Presets map onto the CLI's relative-time syntax (now-<N>h).
    return { start: `now-${win}`, end: 'now' };
  }

  async function submit() {
    setError(null);
    const w = resolveWindow();
    if ('error' in w) { setError(w.error); return; }

    // Range-check the numeric knobs before submit. A negative half-spread synthesizes a
    // crossed book on the backend and banks phantom profit into the "authoritative" ledger,
    // so these bounds mirror the server-side guard in build_config().
    const sp = Number(spread);
    if (!Number.isFinite(sp) || sp < 0 || sp > 0.5) {
      setError('Spread (book half-spread) must be a number between 0 and 0.5.');
      return;
    }
    const dp = Number(depth);
    if (!Number.isFinite(dp) || dp <= 0) {
      setError('Depth must be a positive number of shares.');
      return;
    }
    const cm = Number(commission);
    if (!Number.isFinite(cm) || cm < 0 || cm >= 1) {
      setError('Commission must be a rate between 0 and 1.');
      return;
    }

    const req: BacktestRunRequest = {
      coin,
      start: w.start,
      end: w.end,
      interval,
      spread,
      depth,
      commission,
      llm_score: llmScore,
    };
    const strats = Array.from(selected);
    if (strats.length > 0) req.strategies = strats;

    setSubmitting(true);
    try {
      await onRun(req);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSubmitting(false);
    }
  }

  const disabled = submitting || busy || DEMO_MODE;

  return (
    <div className="card px-5 py-4 space-y-4 border border-indigo-500/20 bg-[#0d0d1a]">
      <div className="flex items-center gap-2">
        <span className="text-base">🧪</span>
        <p className="label-muted text-xs">New Backtest Run</p>
        {busy && (
          <span className="text-[10px] font-mono text-amber-400/80">a run is in progress — one at a time</span>
        )}
      </div>

      <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 gap-4">
        <Field label="Coin">
          <select
            value={coin}
            onChange={e => setCoin(e.target.value)}
            className="bg-[#0a0a12] border border-[#1e1e32] rounded px-2 py-1.5 text-sm font-mono text-gray-200 focus:outline-none focus:border-indigo-500"
          >
            {coinOptions.map(c => <option key={c} value={c}>{c}</option>)}
          </select>
        </Field>

        <Field label="Window">
          <div className="flex flex-wrap gap-1">
            {WINDOWS.map(w => (
              <button
                key={w.value}
                type="button"
                onClick={() => setWin(w.value)}
                className={[
                  'text-[11px] font-mono px-2.5 py-1 rounded border transition-colors',
                  win === w.value
                    ? 'bg-indigo-500/20 border-indigo-500/40 text-indigo-300'
                    : 'bg-[#13131f] border-[#1e1e32] text-gray-500 hover:border-gray-600 hover:text-gray-300',
                ].join(' ')}
              >
                {w.label}
              </button>
            ))}
          </div>
        </Field>

        <Field label="Interval">
          <select
            value={interval}
            onChange={e => setIntervalStr(e.target.value)}
            className="bg-[#0a0a12] border border-[#1e1e32] rounded px-2 py-1.5 text-sm font-mono text-gray-200 focus:outline-none focus:border-indigo-500"
          >
            {['1m', '5m', '15m', '1h'].map(i => <option key={i} value={i}>{i}</option>)}
          </select>
        </Field>

        <Field label="LLM scoring" hint="experimental — needs a provider">
          <button
            type="button"
            onClick={() => setLlmScore(v => !v)}
            className={[
              'text-xs font-mono px-3 py-1.5 rounded border transition-colors w-fit',
              llmScore
                ? 'bg-violet-500/20 border-violet-500/40 text-violet-300'
                : 'bg-[#13131f] border-[#1e1e32] text-gray-500 hover:border-gray-600 hover:text-gray-300',
            ].join(' ')}
          >
            {llmScore ? '🤖 ON' : 'OFF'}
          </button>
        </Field>
      </div>

      {win === 'custom' && (
        <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
          <Field label="Custom start">
            <input
              type="datetime-local"
              value={customStart}
              onChange={e => setCustomStart(e.target.value)}
              className="bg-[#0a0a12] border border-[#1e1e32] rounded px-2 py-1.5 text-sm font-mono text-gray-200 focus:outline-none focus:border-indigo-500"
            />
          </Field>
          <Field label="Custom end">
            <input
              type="datetime-local"
              value={customEnd}
              onChange={e => setCustomEnd(e.target.value)}
              className="bg-[#0a0a12] border border-[#1e1e32] rounded px-2 py-1.5 text-sm font-mono text-gray-200 focus:outline-none focus:border-indigo-500"
            />
          </Field>
        </div>
      )}

      <div className="grid grid-cols-3 gap-4">
        <Field label="Spread (±)" hint="book half-spread (Tier C)">
          <input value={spread} onChange={e => setSpread(e.target.value)} className="input-field w-full text-left" />
        </Field>
        <Field label="Depth (sh/side)" hint="modeled depth (Tier C)">
          <input value={depth} onChange={e => setDepth(e.target.value)} className="input-field w-full text-left" />
        </Field>
        <Field label="Commission" hint="fee rate (0 = none)">
          <input value={commission} onChange={e => setCommission(e.target.value)} className="input-field w-full text-left" />
        </Field>
      </div>

      <Field label="Strategies" hint="none selected → all vipers enabled">
        <div className="flex flex-wrap gap-1.5">
          {STRATS.map(s => (
            <button
              key={s.value}
              type="button"
              onClick={() => toggleStrat(s.value)}
              className={[
                'text-[11px] font-mono px-2.5 py-1 rounded border transition-colors',
                selected.has(s.value)
                  ? 'bg-indigo-500/20 border-indigo-500/40 text-indigo-300'
                  : 'bg-[#13131f] border-[#1e1e32] text-gray-500 hover:border-gray-600 hover:text-gray-300',
              ].join(' ')}
            >
              {s.label}
            </button>
          ))}
        </div>
      </Field>

      {error && (
        <div className="text-xs font-mono text-red-400 bg-red-500/10 border border-red-500/20 rounded px-3 py-2">
          {error}
        </div>
      )}

      <div className="flex items-center gap-3">
        <button
          type="button"
          onClick={submit}
          disabled={disabled}
          className={[
            'text-sm font-mono px-4 py-2 rounded border transition-colors',
            disabled
              ? 'bg-[#13131f] border-[#1e1e32] text-gray-600 cursor-not-allowed'
              : 'bg-indigo-500/20 border-indigo-500/40 text-indigo-200 hover:bg-indigo-500/30',
          ].join(' ')}
        >
          {submitting ? 'Launching…' : '▶ Run Backtest'}
        </button>
        {DEMO_MODE && <span className="text-[10px] font-mono text-gray-600">disabled in demo mode</span>}
      </div>
    </div>
  );
}

// ── Run list ──────────────────────────────────────────────────────────────────

function RunList({
  runs, selectedId, onSelect, loading,
}: {
  runs: { id: string; params: { coin: string; interval: string }; status: BacktestRunStatus; started_at: string }[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  loading: boolean;
}) {
  return (
    <div className="card overflow-hidden">
      <div className="px-4 pt-3 pb-2 flex items-center justify-between">
        <div className="flex items-center gap-2">
          <span className="text-indigo-400 text-base">📜</span>
          <p className="label-muted">Runs</p>
        </div>
        <span className="text-xs font-mono text-gray-600">{loading ? 'Loading…' : `${runs.length}`}</span>
      </div>
      {runs.length === 0 ? (
        <div className="flex items-center justify-center h-24 text-gray-600 text-sm">
          {loading ? 'Loading runs…' : 'No runs yet — launch one above.'}
        </div>
      ) : (
        <div className="divide-y divide-[#1e1e32] max-h-72 overflow-y-auto">
          {runs.map(r => (
            <button
              key={r.id}
              onClick={() => onSelect(r.id)}
              className={[
                'w-full text-left px-4 py-2.5 flex items-center justify-between gap-3 transition-colors',
                selectedId === r.id ? 'bg-[#1a1a2e]' : 'hover:bg-[#161626]',
              ].join(' ')}
            >
              <div className="flex flex-col gap-0.5 min-w-0">
                <span className="text-xs font-mono text-gray-300">
                  {r.params.coin} <span className="text-gray-600">{r.params.interval}</span>
                </span>
                <span className="text-[10px] font-mono text-gray-600">{fmtTs(r.started_at)}</span>
              </div>
              <StatusBadge status={r.status} />
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

// ── Equity curve ──────────────────────────────────────────────────────────────

function EquityCurve({ equity, starting }: { equity: { ts: string; equity: string }[]; starting: number }) {
  const data = useMemo(
    () => equity.map(p => ({ t: new Date(p.ts).getTime(), equity: parseFloat(p.equity) })),
    [equity],
  );
  if (data.length === 0) {
    return <div className="flex items-center justify-center h-40 text-gray-600 text-sm">No equity samples.</div>;
  }
  return (
    <div className="h-64 w-full">
      <ResponsiveContainer width="100%" height="100%">
        <LineChart data={data} margin={{ top: 8, right: 12, bottom: 4, left: 4 }}>
          <CartesianGrid strokeDasharray="3 3" stroke="#1e1e32" />
          <XAxis
            dataKey="t" type="number" domain={['dataMin', 'dataMax']} scale="time"
            tickFormatter={(t) => new Date(t).toLocaleTimeString('en-US', { hour: '2-digit', minute: '2-digit', hour12: false })}
            tick={{ fill: '#6b7280', fontSize: 10, fontFamily: 'monospace' }}
            stroke="#1e1e32"
          />
          <YAxis
            domain={['auto', 'auto']}
            tick={{ fill: '#6b7280', fontSize: 10, fontFamily: 'monospace' }}
            stroke="#1e1e32"
            width={56}
            tickFormatter={(v) => `$${Number(v).toFixed(0)}`}
          />
          <Tooltip
            contentStyle={{ background: '#13131f', border: '1px solid #2e2e4e', borderRadius: 8, fontFamily: 'monospace', fontSize: 11 }}
            labelFormatter={(t) => fmtTs(new Date(t as number).toISOString())}
            formatter={(v: number | string) => [`$${Number(v).toFixed(2)}`, 'Equity']}
          />
          <ReferenceLine y={starting} stroke="#4b5563" strokeDasharray="4 4" />
          <Line type="monotone" dataKey="equity" stroke="#818cf8" strokeWidth={1.6} dot={false} isAnimationActive={false} />
        </LineChart>
      </ResponsiveContainer>
    </div>
  );
}

// ── Trades table ──────────────────────────────────────────────────────────────

function TradesTable({ trades }: { trades: BacktestTrade[] }) {
  if (trades.length === 0) {
    return <div className="text-xs font-mono text-gray-600 px-4 py-3">No trades fired over this window.</div>;
  }
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-xs font-mono">
        <thead>
          <tr className="border-b border-[#1e1e32]">
            {['Strategy', 'Side', 'Kind', 'Entry', 'Exit', 'Shares', 'P&L', 'Reason'].map(h => (
              <th key={h} className="px-3 py-2 text-left text-gray-500 font-normal whitespace-nowrap">{h}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {trades.map((t, i) => {
            const isYes = t.side.toUpperCase() === 'YES';
            return (
              <tr key={i} className="border-b border-[#1e1e32] hover:bg-[#1a1a2e] transition-colors">
                <td className="px-3 py-2 text-gray-300 whitespace-nowrap">{t.strategy}</td>
                <td className={`px-3 py-2 font-semibold ${isYes ? 'text-green-400' : 'text-red-400'}`}>{t.side}</td>
                <td className="px-3 py-2 text-gray-500">{t.kind}</td>
                <td className="px-3 py-2 text-gray-300">{fmtNum(t.entry_price, 4)}</td>
                <td className="px-3 py-2 text-gray-300">{fmtNum(t.exit_price, 4)}</td>
                <td className="px-3 py-2 text-gray-400">{fmtNum(t.shares, 2)}</td>
                <td className={`px-3 py-2 font-semibold ${pnlClass(t.pnl)}`}>{fmtNum(t.pnl, 4)}</td>
                <td className="px-3 py-2 text-gray-500 max-w-[220px] truncate" title={t.reason}>{t.reason}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

// ── Results view ──────────────────────────────────────────────────────────────

function Results({ id }: { id: string }) {
  const { data: run, error } = useSWR(
    ['backtest-run', id],
    () => getBacktestRun(id),
    { refreshInterval: (d) => (d?.status === 'running' ? 2000 : 0) },
  );

  if (error) {
    return <div className="card px-5 py-4 text-sm font-mono text-red-400">Failed to load run: {String(error)}</div>;
  }
  if (!run) {
    return <div className="card px-5 py-4 text-sm text-gray-600">Loading run…</div>;
  }

  if (run.status === 'running') {
    return (
      <div className="card px-5 py-6 flex flex-col items-center justify-center gap-2">
        <span className="h-2.5 w-2.5 rounded-full bg-amber-400 animate-pulse" />
        <p className="text-sm font-mono text-amber-300">Replaying {run.params.coin} {run.params.interval}…</p>
        <p className="text-[11px] font-mono text-gray-600">Fetching candles + funding, driving the real vipers.</p>
      </div>
    );
  }

  if (run.status === 'failed') {
    return (
      <div className="card px-5 py-4 space-y-2">
        <div className="flex items-center gap-2"><StatusBadge status="failed" /><span className="text-sm font-mono text-gray-300">{run.params.coin} {run.params.interval}</span></div>
        <div className="text-xs font-mono text-red-400 bg-red-500/10 border border-red-500/20 rounded px-3 py-2 whitespace-pre-wrap">
          {run.error ?? 'unknown error'}
        </div>
      </div>
    );
  }

  const report = run.report;
  if (!report) {
    return <div className="card px-5 py-4 text-sm text-gray-600">Run complete, but no report was attached.</div>;
  }

  const nl = report.native_ledger;
  const rs = report.rs_backtester;
  const scores = report.llm_scores ?? [];
  const starting = parseFloat(nl.starting_collateral);

  return (
    <div className="space-y-5">
      {/* Header / meta */}
      <div className="card px-5 py-4 border border-indigo-500/20 bg-[#0d0d1a]">
        <div className="flex flex-wrap items-center gap-x-6 gap-y-2">
          <div className="flex items-center gap-2">
            <StatusBadge status="done" />
            <span className="text-sm font-mono text-gray-200">{report.coin} {report.interval}</span>
          </div>
          <span className="text-xs font-mono text-gray-500">
            {report.ticks} ticks · {report.markets} synthetic hourly markets
          </span>
          <span className="text-xs font-mono text-gray-600">
            replayed {fmtMs(report.replayed_start_ms)} → {fmtMs(report.replayed_end_ms)}
          </span>
          <span className="text-xs font-mono text-gray-600">
            spread ±{report.params.spread} · depth {report.params.depth} · commission {report.params.commission}
          </span>
        </div>
      </div>

      {/* Native ledger headline */}
      <div className="grid grid-cols-2 sm:grid-cols-4 gap-3">
        <div className="card px-4 py-3 flex flex-col gap-1">
          <span className="label-muted">Starting</span>
          <span className="stat-value text-gray-200">${fmtNum(nl.starting_collateral)}</span>
        </div>
        <div className="card px-4 py-3 flex flex-col gap-1">
          <span className="label-muted">Realized P&L</span>
          <span className={`stat-value ${pnlClass(nl.realized_pnl)}`}>{parseFloat(nl.realized_pnl) >= 0 ? '+' : ''}${fmtNum(nl.realized_pnl)}</span>
        </div>
        <div className="card px-4 py-3 flex flex-col gap-1">
          <span className="label-muted">Final Equity</span>
          <span className="stat-value text-gray-200">${fmtNum(nl.final_equity)}</span>
        </div>
        <div className="card px-4 py-3 flex flex-col gap-1">
          <span className="label-muted">Closed Trades</span>
          <span className="stat-value text-gray-200">{nl.closed_trades}</span>
        </div>
      </div>

      {/* Equity curve */}
      <div className="card px-4 py-3">
        <div className="flex items-center gap-2 mb-2">
          <span className="text-indigo-400 text-base">📈</span>
          <p className="label-muted">Equity Curve — Native Decimal Ledger</p>
        </div>
        <EquityCurve equity={run.equity ?? []} starting={starting} />
      </div>

      {/* Per-strategy metrics (native ledger) */}
      <div className="card overflow-hidden">
        <div className="px-4 pt-3 pb-2 flex items-center gap-2">
          <span className="text-green-400 text-base">📒</span>
          <p className="label-muted">Per-Strategy — Native Decimal Ledger (authoritative binary settlement)</p>
        </div>
        {nl.per_strategy.length === 0 ? (
          <div className="text-xs font-mono text-gray-600 px-4 py-3">No trades fired — all vipers stood down.</div>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-xs font-mono">
              <thead>
                <tr className="border-b border-[#1e1e32]">
                  {['Strategy', 'Trades', 'Wins', 'Win %', 'P&L ($)'].map(h => (
                    <th key={h} className="px-3 py-2 text-left text-gray-500 font-normal whitespace-nowrap">{h}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {nl.per_strategy.map(s => (
                  <tr key={s.strategy} className="border-b border-[#1e1e32] hover:bg-[#1a1a2e] transition-colors">
                    <td className="px-3 py-2 text-gray-300">{s.strategy}</td>
                    <td className="px-3 py-2 text-gray-400">{s.trades}</td>
                    <td className="px-3 py-2 text-gray-400">{s.wins}</td>
                    <td className="px-3 py-2 text-gray-400">{fmtPct(s.win_rate_pct, 1)}</td>
                    <td className={`px-3 py-2 font-semibold ${pnlClass(s.pnl)}`}>{fmtNum(s.pnl, 2)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* rs-backtester proxy metrics */}
      <div className="card px-4 py-3">
        <div className="flex items-center gap-2 mb-2">
          <span className="text-amber-400 text-base">📐</span>
          <p className="label-muted">rs-backtester — Directional Proxy on the Underlying (NOT the binary payoff)</p>
        </div>
        {rs ? (
          <div className="grid grid-cols-2 sm:grid-cols-5 gap-3 text-xs font-mono">
            <div className="flex flex-col gap-0.5"><span className="text-gray-500">Return</span><span className={pnlClass(rs.return_pct)}>{fmtPct(rs.return_pct)}</span></div>
            <div className="flex flex-col gap-0.5"><span className="text-gray-500">Sharpe</span><span className="text-gray-300">{fmtNum(rs.sharpe, 2)}</span></div>
            <div className="flex flex-col gap-0.5"><span className="text-gray-500">Max DD</span><span className="text-gray-300">{fmtPct(rs.max_drawdown_pct)}</span></div>
            <div className="flex flex-col gap-0.5"><span className="text-gray-500">Win Rate</span><span className="text-gray-300">{fmtPct(rs.win_rate_pct)}</span></div>
            <div className="flex flex-col gap-0.5"><span className="text-gray-500">Trades</span><span className="text-gray-300">{rs.trades_nr ?? '—'}</span></div>
          </div>
        ) : (
          <div className="text-xs font-mono text-gray-600">Proxy skipped (series too short or crate declined the data).</div>
        )}
      </div>

      {/* LLM scores */}
      {scores.length > 0 && (
        <div className="card overflow-hidden">
          <div className="px-4 pt-3 pb-2 flex items-center gap-2">
            <span className="text-violet-400 text-base">🤖</span>
            <p className="label-muted">LLM Conviction — Score vs Realized Outcome</p>
          </div>
          <div className="overflow-x-auto">
            <table className="w-full text-xs font-mono">
              <thead>
                <tr className="border-b border-[#1e1e32]">
                  {['Strategy', 'Side', 'Entry', 'Score', 'Realized P&L', 'Rationale'].map(h => (
                    <th key={h} className="px-3 py-2 text-left text-gray-500 font-normal whitespace-nowrap">{h}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {scores.map((s, i) => (
                  <tr key={i} className="border-b border-[#1e1e32] hover:bg-[#1a1a2e] transition-colors">
                    <td className="px-3 py-2 text-gray-300">{s.strategy}</td>
                    <td className={`px-3 py-2 font-semibold ${s.side.toUpperCase() === 'YES' ? 'text-green-400' : 'text-red-400'}`}>{s.side}</td>
                    <td className="px-3 py-2 text-gray-500">{fmtTs(s.entry_ts)}</td>
                    <td className="px-3 py-2 text-violet-300">{s.score}/100</td>
                    <td className={`px-3 py-2 font-semibold ${pnlClass(s.realized_pnl)}`}>{s.realized_pnl === null ? '—' : fmtNum(s.realized_pnl, 4)}</td>
                    <td className="px-3 py-2 text-gray-500 max-w-[280px] truncate" title={s.rationale}>{s.rationale}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {/* Trades */}
      <div className="card overflow-hidden">
        <div className="px-4 pt-3 pb-2 flex items-center gap-2">
          <span className="text-indigo-400 text-base">🎯</span>
          <p className="label-muted">Trades</p>
        </div>
        <TradesTable trades={run.trades ?? []} />
      </div>

      {/* Fidelity disclaimer — prominent, verbatim from report.json */}
      <div className="card px-4 py-3 border border-amber-500/20 bg-amber-500/[0.03]">
        <div className="flex items-center gap-2 mb-2">
          <span className="text-amber-400 text-base">⚠️</span>
          <p className="label-muted text-amber-300/80">Fidelity Tiers — Read Before Trusting These Numbers</p>
        </div>
        <pre className="text-[11px] font-mono text-amber-200/80 whitespace-pre-wrap leading-relaxed overflow-x-auto">
          {report.fidelity}
        </pre>
      </div>
    </div>
  );
}

// ── Page ──────────────────────────────────────────────────────────────────────

interface Props {
  availableAssets: string[];
}

export default function BacktestPage({ availableAssets }: Props) {
  const [selectedId, setSelectedId] = useState<string | null>(null);

  const { data: runs = [], isLoading, mutate } = useSWR(
    'backtest-runs',
    getBacktestRuns,
    { refreshInterval: (d) => (d?.some(r => r.status === 'running') ? 2000 : 15000) },
  );

  const busy = runs.some(r => r.status === 'running');

  async function handleRun(req: BacktestRunRequest) {
    const { id } = await runBacktest(req);
    await mutate();
    setSelectedId(id);
  }

  // Default the results pane to the newest run once runs load.
  const effectiveId = selectedId ?? (runs.length > 0 ? runs[0].id : null);

  return (
    <div className="space-y-5">
      <div className="card px-5 py-4 border border-indigo-500/20 bg-[#0d0d1a]">
        <p className="label-muted text-xs">🧪 Backtest — Historical Viper Replay</p>
        <p className="text-sm text-gray-400 mt-0.5">
          Replays historical Hyperliquid candles through the real viper strategies with an
          authoritative Decimal ledger and an rs-backtester directional proxy.
          <span className="text-gray-500"> Two labeled PnL views; honest fidelity tiers on every result.</span>
        </p>
      </div>

      <RunForm assets={availableAssets} busy={busy} onRun={handleRun} />

      <div className="grid grid-cols-1 lg:grid-cols-[minmax(0,20rem)_1fr] gap-5 items-start">
        <RunList runs={runs} selectedId={effectiveId} onSelect={setSelectedId} loading={isLoading} />
        <div className="min-w-0">
          {effectiveId
            ? <Results id={effectiveId} />
            : <div className="card px-5 py-8 text-center text-sm text-gray-600">Launch a run to see results here.</div>}
        </div>
      </div>
    </div>
  );
}
