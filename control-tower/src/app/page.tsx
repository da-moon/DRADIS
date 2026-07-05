'use client';

import { useCallback, useEffect, useMemo, useState } from 'react';
import useSWR from 'swr';
import dynamic from 'next/dynamic';

import ViperCard       from '@/components/ViperCard';
import LlmAdvisorCard  from '@/components/LlmAdvisorCard';
import OpenPositionsCard from '@/components/OpenPositionsCard';
import SquadronsPanel  from '@/components/SquadronsPanel';
import SquadronDetailView from '@/components/SquadronDetailView';
import TradelogPage    from '@/components/TradelogPage';
import ErrorBoundary   from '@/components/ErrorBoundary';
import { getAssets, getConfig, getPnlHistory, getTrades, getOpenPositions, getHealth, patchConfig, VIPER_DEFS, getStatus, getLlmRecommendations, getPortfolioValue, getSquadrons, probeBacktest } from '@/lib/api';
import { DEMO_MODE } from '@/lib/demo';
import type { DynamicConfig, SquadronSummary, TradeRow } from '@/lib/types';

// Recharts must be loaded client-side only
const PnlChart = dynamic(() => import('@/components/PnlChart'), { ssr: false });
const TelemetryPage = dynamic(() => import('@/components/TelemetryPage'), { ssr: false });
const BacktestPage = dynamic(() => import('@/components/BacktestPage'), { ssr: false });

// ── Helpers ───────────────────────────────────────────────────────────────────

function fmt$(n: number) {
  return n.toLocaleString('en-US', { style: 'currency', currency: 'USD', minimumFractionDigits: 2 });
}

function fmtPct(n: number) {
  const sign = n >= 0 ? '+' : '';
  return `${sign}${(n * 100).toFixed(2)}%`;
}

// ── Session time helpers ──────────────────────────────────────────────────────

/** Format an ISO-8601 session start as a short "HH:MM" local-time string. */
function fmtSessionTime(iso: string): string {
  try {
    return new Date(iso).toLocaleTimeString(undefined, {
      hour: '2-digit',
      minute: '2-digit',
    });
  } catch {
    return '—';
  }
}

/** Return a human-readable "Xh Ym" uptime string from an ISO-8601 start. */
function fmtUptime(iso: string): string {
  try {
    const secs = Math.floor((Date.now() - new Date(iso).getTime()) / 1000);
    if (secs < 60) return `${secs}s`;
    const mins = Math.floor(secs / 60);
    if (mins < 60) return `${mins}m`;
    const h = Math.floor(mins / 60);
    const m = mins % 60;
    return m > 0 ? `${h}h ${m}m` : `${h}h`;
  } catch {
    return '—';
  }
}

// ── Session badge ─────────────────────────────────────────────────────────────

function SessionBadge({ startedAt }: { startedAt?: string }) {
  // Re-render every minute so the uptime counter stays current.
  const [, setTick] = useState(0);
  useEffect(() => {
    const id = setInterval(() => setTick(t => t + 1), 60_000);
    return () => clearInterval(id);
  }, []);

  if (!startedAt) return null;
  const uptime = fmtUptime(startedAt);
  const startTime = fmtSessionTime(startedAt);
  return (
    <div
      className="hidden sm:flex items-center gap-1.5 text-xs font-mono text-gray-500 cursor-default"
      title={`Session started: ${startedAt}`}
    >
      <span className="text-gray-600">⏱</span>
      <span>
        <span className="text-gray-500">Session</span>
        <span className="text-gray-400 ml-1">{startTime}</span>
        <span className="text-gray-600 ml-1">({uptime})</span>
      </span>
    </div>
  );
}

// ── Stat card ─────────────────────────────────────────────────────────────────

function StatCard({ label, value, sub, valueClass = '' }: {
  label: string; value: string; sub?: string; valueClass?: string;
}) {
  return (
    <div className="card px-4 py-3 flex flex-col gap-1">
      <span className="label-muted">{label}</span>
      <span className={`stat-value ${valueClass}`}>{value}</span>
      {sub && <span className="text-xs text-gray-500">{sub}</span>}
    </div>
  );
}

// ── Ghost mode banner ─────────────────────────────────────────────────────────

function GhostBanner({ ghost }: { ghost: boolean }) {
  return ghost ? (
    <div className="bg-amber-500/10 border border-amber-500/30 rounded-lg px-4 py-2 text-amber-300 text-xs font-mono flex items-center gap-2">
      <span className="text-base">👻</span>
      <span>
        <strong>GHOST MODE ACTIVE</strong> — new orders are simulated (paper), no real CLOB calls.
        Toggling affects NEW orders within ~30 seconds; open positions keep the mode they were opened with.
      </span>
    </div>
  ) : null;
}

// ── Paper equity sparkline ────────────────────────────────────────────────────

/** Tiny inline-SVG sparkline of the cumulative paper-equity series. No deps. */
function PaperSparkline({ series, width = 132, height = 34 }: {
  series: number[]; width?: number; height?: number;
}) {
  if (series.length < 2) return null;
  const min = Math.min(...series);
  const max = Math.max(...series);
  const span = max - min || 1;
  const pad = 2;
  const stepX = (width - pad * 2) / (series.length - 1);
  const y = (v: number) => pad + (height - pad * 2) * (1 - (v - min) / span);
  const pts = series.map((v, i) => `${(pad + i * stepX).toFixed(2)},${y(v).toFixed(2)}`).join(' ');
  const up = series[series.length - 1] >= series[0];
  const stroke = up ? '#4ade80' : '#f87171';
  const baseY = y(series[0]);
  return (
    <svg width={width} height={height} className="overflow-visible" aria-hidden="true">
      <line x1={pad} y1={baseY} x2={width - pad} y2={baseY} stroke="#334155" strokeWidth={1} strokeDasharray="3 3" />
      <polyline points={pts} fill="none" stroke={stroke} strokeWidth={1.5} strokeLinejoin="round" strokeLinecap="round" />
    </svg>
  );
}

// ── Paper (ghost) ledger card ─────────────────────────────────────────────────

function PaperLedgerCard({
  paperPnl, paperBalance, paperTrades,
}: {
  paperPnl: number; paperBalance: number;
  /** Ghost trades (ascending by ts) with realized pnl, from the already-fetched trades. */
  paperTrades: { ts: number; pnl: number }[];
}) {
  const positive = paperPnl >= 0;

  // Cumulative paper-equity series, ANCHORED so its endpoint equals the current Paper
  // Balance headline. We can't anchor at a fixed all-time starting balance and sum
  // forward: the fetched ghost trades are only the newest ~60 per asset (and may span
  // prior sessions), whereas paperBalance/paperPnl are the current in-memory ledger, so a
  // forward-summed curve's endpoint would silently disagree with the headline once trades
  // are truncated or a session rolls over. Instead we walk BACKWARD from paperBalance:
  // last point = balance, each earlier point subtracts the following trade's pnl. The
  // endpoint is always consistent with the headline; the curve shows the shape of the
  // fetched window. Purely client-side from the already-fetched trades — no backend.
  const series = useMemo(() => {
    const total = paperTrades.reduce((sum, t) => sum + t.pnl, 0);
    let acc = paperBalance - total;
    const pts = [acc];
    for (const t of paperTrades) {
      acc += t.pnl;
      pts.push(acc);
    }
    return pts;
  }, [paperBalance, paperTrades]);

  return (
    <div className="card px-5 py-3 flex flex-wrap items-center gap-x-8 gap-y-2 border border-amber-500/20 bg-[#0d0d1a]">
      <div className="flex items-center gap-2">
        <span className="text-base">👻</span>
        <span className="label-muted text-xs">Paper Ledger</span>
        <span className="text-[10px] font-mono bg-amber-500/10 text-amber-400 border border-amber-500/20 rounded px-1.5 py-0.5">
          simulated
        </span>
      </div>
      <div className="flex flex-col gap-0.5">
        <span className="text-gray-500 text-xs font-mono">Paper P&amp;L</span>
        <span className={`text-sm font-mono ${positive ? 'text-green-400' : 'text-red-400'}`}>
          {(positive ? '+' : '') + fmt$(paperPnl)}
        </span>
      </div>
      <div className="flex flex-col gap-0.5">
        <span className="text-gray-500 text-xs font-mono">Paper Balance</span>
        <span className="text-sm font-mono text-gray-300">{fmt$(paperBalance)}</span>
      </div>
      {series.length >= 2 && (
        <div className="flex flex-col gap-0.5 ml-auto">
          <span className="text-gray-500 text-[10px] font-mono">Paper Equity ({series.length - 1} trades)</span>
          <PaperSparkline series={series} />
        </div>
      )}
    </div>
  );
}

// ── Asset selector tabs ───────────────────────────────────────────────────────

const ASSET_EMOJI: Record<string, string> = {
  btc: '₿',
  eth: 'Ξ',
  sol: '◎',
};

function AssetTabs({
  assets,
  selected,
  onChange,
}: {
  assets: string[];
  selected: string;
  onChange: (a: string) => void;
}) {
  if (assets.length <= 1) return null;
  return (
    <div className="flex items-center gap-1">
      {assets.map((a) => {
        const active = a === selected;
        return (
          <button
            key={a}
            onClick={() => onChange(a)}
            className={[
              'flex items-center gap-1.5 text-xs font-mono px-3 py-1.5 rounded-lg border transition-colors',
              active
                ? 'bg-indigo-500/20 border-indigo-500/40 text-indigo-300'
                : 'bg-[#13131f] border-[#1e1e32] text-gray-500 hover:border-gray-600 hover:text-gray-300',
            ].join(' ')}
          >
            <span>{ASSET_EMOJI[a] ?? '◈'}</span>
            <span>{a.toUpperCase()}</span>
          </button>
        );
      })}
    </div>
  );
}

// ── Portfolio value banner ────────────────────────────────────────────────────

function PortfolioValueBanner({
  totalValue, collateral, positionsValue, unrealizedPnl,
  positionCount, sessionPnl, ghostMode, pricesLive, isLoading,
}: {
  totalValue: number; collateral: number; positionsValue: number;
  unrealizedPnl: number; positionCount: number; sessionPnl: number;
  ghostMode?: boolean; pricesLive: boolean; isLoading: boolean;
}) {
  // The true session delta is realized P&L + unrealized P&L.
  // This is correct whether or not positions were carried in from a prior session,
  // because it does NOT assume the starting portfolio was just cash — it derives
  // the starting portfolio value as (totalValue - delta) rather than using the
  // raw collateral snapshot which omits the cost basis of any open positions.
  const delta                = sessionPnl + unrealizedPnl;
  const startingPortfolioVal = totalValue - delta;
  const deltaPct             = startingPortfolioVal > 0 ? delta / startingPortfolioVal : 0;
  const isPositive           = delta >= 0;

  return (
    <div className="card px-5 py-4 flex flex-col sm:flex-row sm:items-center gap-3 border border-indigo-500/20 bg-[#0d0d1a]">
      {/* Main figure */}
      <div className="flex flex-col flex-1 min-w-0">
        <div className="flex items-center gap-2 mb-0.5">
          <span className="label-muted text-xs">Portfolio Value</span>
          {!pricesLive && (
            <span className="text-[10px] font-mono bg-yellow-500/10 text-yellow-400 border border-yellow-500/20 rounded px-1.5 py-0.5">
              ⚡ cached prices
            </span>
          )}
          {ghostMode && (
            <span className="text-[10px] font-mono bg-amber-500/10 text-amber-400 border border-amber-500/20 rounded px-1.5 py-0.5">
              virtual
            </span>
          )}
        </div>
        <span className={`text-3xl font-mono font-bold tracking-tight ${isLoading ? 'text-gray-600' : 'text-white'}`}>
          {isLoading ? '——' : fmt$(totalValue)}
        </span>
        {!isLoading && startingPortfolioVal > 0 && (
          <span className={`text-sm font-mono mt-0.5 ${isPositive ? 'text-green-400' : 'text-red-400'}`}>
            {isPositive ? '▲' : '▼'} {fmt$(Math.abs(delta))} ({fmtPct(Math.abs(deltaPct))}) vs session start
          </span>
        )}
      </div>

      {/* Breakdown */}
      <div className="flex gap-4 sm:gap-6 text-xs font-mono flex-wrap">
        <div className="flex flex-col gap-0.5">
          <span className="text-gray-500">Cash</span>
          <span className="text-gray-300">{isLoading ? '—' : fmt$(collateral)}</span>
        </div>
        <div className="flex flex-col gap-0.5">
          <span className="text-gray-500">Positions</span>
          <span className="text-gray-300">{isLoading ? '—' : fmt$(positionsValue)}</span>
          {positionCount > 0 && <span className="text-gray-600">{positionCount} open</span>}
        </div>
        <div className="flex flex-col gap-0.5">
          <span className="text-gray-500">Unrealized P&L</span>
          <span className={isLoading ? 'text-gray-600' : unrealizedPnl >= 0 ? 'text-green-400' : 'text-red-400'}>
            {isLoading ? '—' : (unrealizedPnl >= 0 ? '+' : '') + fmt$(unrealizedPnl)}
          </span>
        </div>
      </div>
    </div>
  );
}

// ── Main page ─────────────────────────────────────────────────────────────────

// ── Top-level nav ─────────────────────────────────────────────────────────────

type AppView = 'main' | 'telemetry' | 'tradelog' | 'backtest';

function NavTabs({
  active,
  onChange,
  showBacktest,
}: {
  active: AppView;
  onChange: (v: AppView) => void;
  showBacktest: boolean;
}) {
  const tabs: { id: AppView; label: string; icon: string }[] = [
    { id: 'main',      label: 'Main',      icon: '🗺️' },
    { id: 'telemetry', label: 'Telemetry', icon: '📡' },
    { id: 'tradelog',  label: 'Tradelog',  icon: '📋' },
    // Backtest tab only when the feature-gated backend endpoint is present.
    ...(showBacktest ? [{ id: 'backtest' as AppView, label: 'Backtest', icon: '🧪' }] : []),
  ];
  return (
    <div className="flex items-center gap-1">
      {tabs.map(t => (
        <button
          key={t.id}
          onClick={() => onChange(t.id)}
          className={[
            'flex items-center gap-1.5 text-xs font-mono px-3 py-1.5 rounded-lg border transition-colors',
            active === t.id
              ? 'bg-indigo-500/20 border-indigo-500/40 text-indigo-300'
              : 'bg-[#13131f] border-[#1e1e32] text-gray-500 hover:border-gray-600 hover:text-gray-300',
          ].join(' ')}
        >
          <span>{t.icon}</span>
          <span>{t.label}</span>
        </button>
      ))}
    </div>
  );
}

export default function DashboardPage() {
  // ── Top-level view (Main vs Tradelog) ───────────────────────────────────────
  const [activeView, setActiveView] = useState<AppView>('main');

  // ── Squadron drill-down state ────────────────────────────────────────────────
  const [focusedSquadronId, setFocusedSquadronId] = useState<string | null>(null);

  // ── Asset selector — populated from GET /api/assets on first load ───────────
  const { data: availableAssets = [] } = useSWR('assets', getAssets, {
    refreshInterval: 0,
    revalidateOnFocus: false,
    // Seed a sensible default while the request is in-flight
    fallbackData: [],
  });

  // Active asset: default to first available or 'btc'.
  const [selectedAsset, setSelectedAsset] = useState<string>('');
  // Resolve the effective asset for API calls (empty string → primary pool).
  const asset = selectedAsset || availableAssets[0] || '';

  const { data: config, mutate: refreshConfig, isLoading: configLoading } =
    useSWR('config', getConfig, { refreshInterval: 0, revalidateOnFocus: false });

  // CAG-level P&L history: fetch global aggregated history (all assets) for main dashboard
  const { data: pnl, isLoading: pnlLoading } =
    useSWR('pnl-global', () => getPnlHistory(1440), { refreshInterval: 60_000 });

  const { data: trades, isLoading: tradesLoading } =
    useSWR(['trades', asset], () => getTrades(60, asset), { refreshInterval: 15_000 });

  // Open positions polled every 15s — same cadence as trades so the activity log stays fresh.
  const { data: openPositions, isLoading: positionsLoading } =
    useSWR(['positions', asset], () => getOpenPositions(asset), { refreshInterval: 15_000 });

  // For chart markers: fetch ALL trades/positions across all assets (not filtered by selected asset)
  const { data: allTrades } =
    useSWR('trades-all', async () => {
      if (availableAssets.length === 0) return [];
      const results = await Promise.all(availableAssets.map(a => getTrades(60, a)));
      return results.flat();
    }, { refreshInterval: 15_000 });

  const { data: allOpenPositions } =
    useSWR('positions-all', async () => {
      if (availableAssets.length === 0) return [];
      const results = await Promise.all(availableAssets.map(a => getOpenPositions(a)));
      return results.flat();
    }, { refreshInterval: 15_000 });

  const { data: health } =
    useSWR('health', getHealth, { refreshInterval: 10_000 });

  const { data: status } =
    useSWR('status', getStatus, { refreshInterval: 30_000 });

  // Poll every 5 minutes — recommendations only arrive every 30 min at most.
  // Global LLM Advisor reads ALL asset databases and writes to primary pool,
  // so we fetch without an asset filter (always reads from primary).
  const { data: llmRecs, isLoading: llmLoading } =
    useSWR('llmRecs', () => getLlmRecommendations(10), { refreshInterval: 300_000 });

  // Portfolio value: collateral + live mark-to-market on open positions.
  // Refresh every 30 s so the number stays fresh without hammering Polymarket CLOB.
  const { data: portfolio, isLoading: portfolioLoading } =
    useSWR('portfolio', getPortfolioValue, { refreshInterval: 30_000 });

  // CAG squadron registry — refresh every 10 s to catch state transitions quickly.
  const { data: squadrons, isLoading: squadronsLoading } =
    useSWR('squadrons', getSquadrons, { refreshInterval: 10_000 });

  // Probe once whether the feature-gated backtest API is present. Default builds
  // omit those routes (404) → the Backtest tab stays hidden.
  const { data: backtestAvailable = false } =
    useSWR('backtest-available', probeBacktest, { refreshInterval: 0, revalidateOnFocus: false });

  // If the tab is hidden but somehow active (e.g. backend restarted without the
  // feature), fall back to Main so we never render a dead view.
  useEffect(() => {
    if (activeView === 'backtest' && !backtestAvailable) setActiveView('main');
  }, [activeView, backtestAvailable]);

  // ── Stats derived from P&L history ──────────────────────────────────────────
  const latestSnap  = pnl?.[0];
  const oldestSnap  = pnl?.[pnl.length - 1];
  const startingBal = oldestSnap  ? parseFloat(oldestSnap.collateral)  : 0;
  const sessionPnl  = latestSnap  ? parseFloat(latestSnap.session_pnl) : 0;
  const sessionPct  = startingBal > 0 ? sessionPnl / startingBal : 0;
  // In ghost mode, the on-chain pUSD balance never changes (no real orders are placed),
  // so we derive the virtual current balance as startingBal + accumulated session P&L.
  // In live mode, use the actual on-chain collateral from the latest snapshot.
  const currentBal  = config?.ghost_mode
    ? startingBal + sessionPnl
    : (latestSnap ? parseFloat(latestSnap.collateral) : 0);

  // ── Patch handler ────────────────────────────────────────────────────────────
  const handlePatch = useCallback(async (patch: Partial<DynamicConfig>) => {
    if (DEMO_MODE) return;
    await patchConfig(patch);
    await refreshConfig();
  }, [refreshConfig]);

  const isConnected = health === 'ok';

  // ── Squadron navigation ────────────────────────────────────────────────────
  const handleSquadronClick = useCallback((sq: SquadronSummary) => {
    setFocusedSquadronId(sq.id);
  }, []);

  const handleBackToCag = useCallback(() => {
    setFocusedSquadronId(null);
  }, []);

  const focusedSquadron = squadrons?.find((s) => s.id === focusedSquadronId);

  // ── Render squadron detail view if one is selected ─────────────────────────
  if (focusedSquadron) {
    return (
      <div className="min-h-screen bg-[#0a0a12]">
        <header className="sticky top-0 z-10 border-b border-[#1e1e32] bg-[#0a0a12]/90 backdrop-blur-sm px-6 py-3">
          <div className="max-w-7xl mx-auto relative flex items-center justify-between gap-4">
            {/* Logo + nav */}
            <div className="flex items-center gap-3">
              <div className="flex items-center gap-1.5">
                <span className="font-mono font-bold text-lg tracking-wide text-indigo-400">DRADIS</span>
                <span className="text-gray-600 text-lg">|</span>
              </div>
              <NavTabs active={activeView} onChange={(v) => { setActiveView(v); setFocusedSquadronId(null); }} showBacktest={backtestAvailable} />
            </div>

            {/* Center — BSG motto */}
            <div className="absolute left-1/2 -translate-x-1/2 hidden md:block pointer-events-none select-none">
              <span className="font-serif italic text-gray-300 text-base tracking-wide">Good Hunting</span>
            </div>

            {/* Right cluster */}
            <div className="flex items-center gap-3">
              <SessionBadge startedAt={status?.session_started_at} />
              <div className="flex items-center gap-1.5">
                <span className={`h-2 w-2 rounded-full ${isConnected ? 'bg-green-400 animate-pulse' : 'bg-red-500'}`} />
                <span className={`text-xs font-mono ${isConnected ? 'text-green-400' : 'text-red-400'}`}>
                  {isConnected ? 'LIVE' : 'OFFLINE'}
                </span>
              </div>
              {config && !DEMO_MODE && (
                <button
                  onClick={() => handlePatch({ ghost_mode: !config.ghost_mode })}
                  title="Affects NEW orders within ~30 seconds. Open positions keep the mode they were opened with (grandfathered); ghost positions stay simulated."
                  className={[
                    'flex items-center gap-2 text-xs font-mono px-3 py-1.5 rounded-lg border transition-colors',
                    config.ghost_mode
                      ? 'bg-amber-500/10 border-amber-500/30 text-amber-300 hover:bg-amber-500/20'
                      : 'bg-[#13131f] border-[#1e1e32] text-gray-400 hover:border-gray-600',
                  ].join(' ')}
                >
                  <span>{config.ghost_mode ? '' : '⚡'}</span>
                  <span>{config.ghost_mode ? 'GHOST' : 'LIVE'}</span>
                </button>
              )}
            </div>
          </div>
        </header>

        <main className="max-w-7xl mx-auto px-4 sm:px-6 py-6">
          {config?.ghost_mode && <GhostBanner ghost />}
          <SquadronDetailView squadron={focusedSquadron} onBack={handleBackToCag} />
          <footer className="text-center text-xs text-gray-700 pb-4 font-mono mt-12">
            DRADIS Control Tower  Polymarket CLOB Orchestrator {' '}
            <span className="text-gray-600">So say we all.</span>
          </footer>
        </main>
      </div>
    );
  }

  // ── Render CAG overview (default) ──────────────────────────────────────────

  return (
    <div className="min-h-screen bg-[#0a0a12]">
      {/* ── Header ─────────────────────────────────────────────────────────── */}
      <header className="sticky top-0 z-10 border-b border-[#1e1e32] bg-[#0a0a12]/90 backdrop-blur-sm px-6 py-3">
        <div className="max-w-7xl mx-auto relative flex items-center justify-between gap-4">
          {/* Logo + nav tabs */}
          <div className="flex items-center gap-3">
            <div className="flex items-center gap-1.5">
              <span className="font-mono font-bold text-lg tracking-wide text-indigo-400">DRADIS</span>
              <span className="text-gray-600 text-lg">|</span>
            </div>
            <NavTabs active={activeView} onChange={setActiveView} showBacktest={backtestAvailable} />
          </div>

          {/* Center — BSG motto */}
          <div className="absolute left-1/2 -translate-x-1/2 hidden md:block pointer-events-none select-none">
            <span className="font-serif italic text-gray-300 text-base tracking-wide">Good Hunting</span>
          </div>

          {/* Right cluster */}
          <div className="flex items-center gap-3">
            <SessionBadge startedAt={status?.session_started_at} />
            <div className="flex items-center gap-1.5">
              <span className={`h-2 w-2 rounded-full ${isConnected ? 'bg-green-400 animate-pulse' : 'bg-red-500'}`} />
              <span className={`text-xs font-mono ${isConnected ? 'text-green-400' : 'text-red-400'}`}>
                {isConnected ? 'LIVE' : 'OFFLINE'}
              </span>
            </div>
            {config && !DEMO_MODE && (
              <button
                onClick={() => handlePatch({ ghost_mode: !config.ghost_mode })}
                title="Affects NEW orders within ~30 seconds. Open positions keep the mode they were opened with (grandfathered); ghost positions stay simulated."
                className={[
                  'flex items-center gap-2 text-xs font-mono px-3 py-1.5 rounded-lg border transition-colors',
                  config.ghost_mode
                    ? 'bg-amber-500/10 border-amber-500/30 text-amber-300 hover:bg-amber-500/20'
                    : 'bg-[#13131f] border-[#1e1e32] text-gray-400 hover:border-gray-600',
                ].join(' ')}
              >
                <span>{config.ghost_mode ? '' : '⚡'}</span>
                <span>{config.ghost_mode ? 'GHOST' : 'LIVE'}</span>
              </button>
            )}
          </div>
        </div>
      </header>

      {/* ── Tradelog view ──────────────────────────────────────────────────── */}
      {activeView === 'tradelog' && (
        <main className="max-w-7xl mx-auto px-4 sm:px-6 py-6 space-y-6">
          {config?.ghost_mode && <GhostBanner ghost />}
          <TradelogPage availableAssets={availableAssets} />
          <footer className="text-center text-xs text-gray-700 pb-4 font-mono">
            DRADIS Control Tower  Polymarket CLOB Orchestrator {' '}
            <span className="text-gray-600">So say we all.</span>
          </footer>
        </main>
      )}

      {/* ── Telemetry view ─────────────────────────────────────────────────── */}
      {activeView === 'telemetry' && (
        <main className="max-w-7xl mx-auto px-4 sm:px-6 py-6 space-y-6">
          {config?.ghost_mode && <GhostBanner ghost />}
          <ErrorBoundary label="Telemetry">
            <TelemetryPage availableAssets={availableAssets} />
          </ErrorBoundary>
          <footer className="text-center text-xs text-gray-700 pb-4 font-mono">
            DRADIS Control Tower  Polymarket CLOB Orchestrator {' '}
            <span className="text-gray-600">So say we all.</span>
          </footer>
        </main>
      )}

      {/* ── Backtest view (feature-gated backend) ──────────────────────────── */}
      {activeView === 'backtest' && backtestAvailable && (
        <main className="max-w-7xl mx-auto px-4 sm:px-6 py-6 space-y-6">
          {config?.ghost_mode && <GhostBanner ghost />}
          <ErrorBoundary label="Backtest">
            <BacktestPage availableAssets={availableAssets} />
          </ErrorBoundary>
          <footer className="text-center text-xs text-gray-700 pb-4 font-mono">
            DRADIS Control Tower  Polymarket CLOB Orchestrator {' '}
            <span className="text-gray-600">So say we all.</span>
          </footer>
        </main>
      )}

      {/* ── Main view ──────────────────────────────────────────────────────── */}
      {activeView === 'main' && (
      <main className="max-w-7xl mx-auto px-4 sm:px-6 py-6 space-y-6">

        {/* Ghost mode banner */}
        {config?.ghost_mode && <GhostBanner ghost />}

        {/* ── Portfolio Value Banner ─────────────────────────────────── */}
        <PortfolioValueBanner
          totalValue={portfolioLoading ? 0 : parseFloat(portfolio?.total_value ?? '0')}
          collateral={portfolioLoading ? 0 : parseFloat(portfolio?.collateral ?? '0')}
          positionsValue={portfolioLoading ? 0 : parseFloat(portfolio?.positions_value ?? '0')}
          unrealizedPnl={portfolioLoading ? 0 : parseFloat(portfolio?.unrealized_pnl ?? '0')}
          positionCount={portfolio?.position_count ?? 0}
          sessionPnl={pnlLoading ? 0 : sessionPnl}
          ghostMode={config?.ghost_mode}
          pricesLive={portfolio?.prices_live ?? true}
          isLoading={portfolioLoading}
        />

        {/* ── Paper (ghost) ledger — shown when ghosting or any paper activity ── */}
        {(() => {
          const paperPnl = parseFloat(status?.paper_pnl ?? '0');
          const paperBalance = parseFloat(status?.paper_balance ?? '0');
          // Show when ghosting or once any paper activity has realized P&L. NOTE: do not
          // test `paperBalance !== 0` — the ledger is seeded to PAPER_STARTING_COLLATERAL
          // (1000) per squadron session, so it is always non-zero even for a pure-live
          // operator; that made the card render permanently.
          const show = (config?.ghost_mode ?? false) || paperPnl !== 0;
          // Ghost trades across all assets, ascending by ts, for the equity sparkline.
          const paperTrades = ((allTrades ?? []) as TradeRow[])
            .filter(t => t.ghost_mode)
            .map(t => ({ ts: new Date(t.ts).getTime(), pnl: parseFloat(t.pnl) }))
            .filter(t => Number.isFinite(t.ts) && Number.isFinite(t.pnl))
            .sort((a, b) => a.ts - b.ts);
          return show ? <PaperLedgerCard paperPnl={paperPnl} paperBalance={paperBalance} paperTrades={paperTrades} /> : null;
        })()}

        {/* ── Portfolio History Chart (CAG-level) ───────────────────────── */}
        {pnlLoading ? (
          <div className="card p-6 flex items-center justify-center h-64 text-gray-600 text-sm">
            Loading portfolio history…
          </div>
        ) : (
          <PnlChart
            data={pnl ?? []}
            startingBalance={startingBal}
            ghostMode={config?.ghost_mode}
            currentPortfolio={portfolio}
            trades={allTrades ?? []}
            openPositions={allOpenPositions ?? []}
          />
        )}

        {/* ── CAG-level stats ───────────────────────────────────────────── */}
        <div className="grid grid-cols-2 sm:grid-cols-4 gap-3">
          <StatCard
            label="Active Assets"
            value={String(availableAssets.length || 1)}
            sub="multi-asset ops"
          />
          <StatCard
            label="Active Squadrons"
            value={String(squadrons?.filter(s => s.state === 'PATROLLING' || s.state === 'DEPLOYED').length ?? 0)}
            sub="deployed + patrolling"
          />
          <StatCard
            label="Session P&L"
            value={fmt$(sessionPnl)}
            sub={fmtPct(sessionPct)}
            valueClass={sessionPnl >= 0 ? 'text-green-400' : 'text-red-400'}
          />
          <StatCard
            label="Total Squadrons"
            value={String(squadrons?.length ?? 0)}
            sub="all states"
          />
        </div>

        {/* ── LLM Advisor ───────────────────────────────────────────────── */}
        <LlmAdvisorCard
          recommendations={llmRecs ?? []}
          isLoading={llmLoading}
          advisorEnabled={true}
          llmProvider={status?.llm_provider}
          llmModel={status?.llm_model}
        />

        {/* ── CAG Squadron Registry ─────────────────────────────────────── */}
        <section>
          <div className="flex items-center justify-between mb-3">
            <p className="label-muted">Squadron Registry</p>
            <span className="text-xs text-gray-600 font-mono">
              Click a squadron to view details, raptors, vipers, and trades
            </span>
          </div>
          <SquadronsPanel
            squadrons={squadrons ?? []}
            isLoading={squadronsLoading}
            onSquadronClick={handleSquadronClick}
          />
        </section>

        {/* Footer */}
        <footer className="text-center text-xs text-gray-700 pb-4 font-mono">
          DRADIS Control Tower  Polymarket CLOB Orchestrator {' '}
          <span className="text-gray-600">So say we all.</span>
        </footer>
      </main>
      )}
    </div>
  );
}
