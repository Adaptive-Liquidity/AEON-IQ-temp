"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Archive,
  ArchiveRestore,
  Clock,
  FilePlus2,
  History,
  Lock,
  Pencil,
  RefreshCw,
  Search,
  ShieldAlert,
  Sparkles,
  Waypoints,
} from "lucide-react";

// ── Types (mirror src/api.rs LifecycleEventDto + the /lifecycle envelope) ──────

interface LifecycleEvent {
  event_type: string;
  memory_id: string | null;
  timestamp: string;
  detail: Record<string, unknown>;
}

interface LifecycleResponse {
  agent_id: string;
  events: LifecycleEvent[];
  total: number;
  limit: number;
  offset: number;
}

const PAGE_SIZE = 50;

// ── Event-type presentation ─────────────────────────────────────────────────────
// One entry per event_type emitted by fetch_lifecycle_events() in src/memory/store.rs.

type EventStyle = {
  label: string;
  icon: typeof FilePlus2;
  dot: string; // timeline dot colour
  chip: string; // pill colour
};

const EVENT_STYLES: Record<string, EventStyle> = {
  created: {
    label: "Created",
    icon: FilePlus2,
    dot: "bg-green-500 border-green-300/40",
    chip: "bg-green-500/20 text-green-300 border-green-500/30",
  },
  modified: {
    label: "Modified",
    icon: Pencil,
    dot: "bg-blue-500 border-blue-300/40",
    chip: "bg-blue-500/20 text-blue-300 border-blue-500/30",
  },
  status_changed: {
    label: "Status changed",
    icon: ShieldAlert,
    dot: "bg-orange-500 border-orange-300/40",
    chip: "bg-orange-500/20 text-orange-300 border-orange-500/30",
  },
  narrative_generated: {
    label: "Narrative generated",
    icon: Sparkles,
    dot: "bg-purple-500 border-purple-300/40",
    chip: "bg-purple-500/20 text-purple-300 border-purple-500/30",
  },
  consolidation: {
    label: "Consolidation",
    icon: Archive,
    dot: "bg-amber-500 border-amber-300/40",
    chip: "bg-amber-500/20 text-amber-300 border-amber-500/30",
  },
  batch_restored: {
    label: "Batch restored",
    icon: ArchiveRestore,
    dot: "bg-emerald-500 border-emerald-300/40",
    chip: "bg-emerald-500/20 text-emerald-300 border-emerald-500/30",
  },
  retrieval: {
    label: "Retrieval",
    icon: Search,
    dot: "bg-zinc-500 border-zinc-300/40",
    chip: "bg-zinc-700 text-zinc-300 border-zinc-600",
  },
};

function styleFor(eventType: string): EventStyle {
  return (
    EVENT_STYLES[eventType] ?? {
      label: eventType,
      icon: History,
      dot: "bg-zinc-500 border-zinc-300/40",
      chip: "bg-zinc-700 text-zinc-300 border-zinc-600",
    }
  );
}

const STATUS_COLORS: Record<string, string> = {
  active: "text-green-400",
  candidate: "text-yellow-400",
  quarantined: "text-orange-400",
  suppressed: "text-red-400",
  restored: "text-emerald-400",
};

function relative(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime();
  if (diff < 60_000) return `${Math.round(diff / 1000)}s ago`;
  if (diff < 3_600_000) return `${Math.round(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.round(diff / 3_600_000)}h ago`;
  return `${Math.round(diff / 86_400_000)}d ago`;
}

function str(v: unknown): string | null {
  return v === null || v === undefined ? null : String(v);
}

function num(v: unknown): number | null {
  return typeof v === "number" ? v : null;
}

// ── Component ──────────────────────────────────────────────────────────────────

export default function LifecycleClient({
  userAgentId,
  isAdmin,
}: {
  userAgentId: string;
  isAdmin: boolean;
}) {
  const [agentId, setAgentId] = useState(userAgentId);
  const [events, setEvents] = useState<LifecycleEvent[]>([]);
  const [total, setTotal] = useState(0);
  const [loaded, setLoaded] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!isAdmin) setAgentId(userAgentId);
  }, [userAgentId, isAdmin]);

  // Load a page. offset 0 replaces the feed; offset > 0 appends (load more).
  const load = useCallback(
    async (offset: number) => {
      if (!agentId.trim()) return;
      setLoading(true);
      setError(null);
      try {
        const url = `/api/agents/${encodeURIComponent(
          agentId
        )}/lifecycle?limit=${PAGE_SIZE}&offset=${offset}`;
        const res = await fetch(url);
        if (!res.ok) throw new Error(await res.text());
        const data: LifecycleResponse = await res.json();
        const page = data.events ?? [];
        setEvents((prev) => (offset === 0 ? page : [...prev, ...page]));
        setTotal(data.total ?? 0);
        setLoaded(true);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setLoading(false);
      }
    },
    [agentId]
  );

  // Reset + auto-load whenever the agent changes.
  useEffect(() => {
    setEvents([]);
    setTotal(0);
    setLoaded(false);
    if (agentId.trim()) load(0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentId]);

  const hasMore = events.length < total;

  const counts = useMemo(() => {
    const c: Record<string, number> = {};
    for (const e of events) c[e.event_type] = (c[e.event_type] ?? 0) + 1;
    return c;
  }, [events]);

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-bold flex items-center gap-2">
          <Waypoints className="w-6 h-6 text-purple-400" />
          Memory Lifecycle
        </h1>
        <p className="text-zinc-400 text-sm mt-1">
          Unified, chronological feed of every memory-lifecycle event for an
          agent — creation, modification, status change, narrative generation,
          consolidation / restoration, and retrieval. Newest first.
        </p>
      </div>

      {/* Agent ID */}
      <div className="rounded-xl border border-zinc-800 bg-zinc-900 p-4">
        <div className="flex items-center justify-between mb-1">
          <label className="text-xs text-zinc-400">Agent ID</label>
          {!isAdmin && (
            <span className="flex items-center gap-1 text-xs text-zinc-600">
              <Lock className="w-3 h-3" />
              scoped to your account
            </span>
          )}
        </div>
        <input
          value={agentId}
          onChange={isAdmin ? (e) => setAgentId(e.target.value) : undefined}
          readOnly={!isAdmin}
          placeholder="e.g. my-agent-001"
          className={`w-full bg-zinc-800 border border-zinc-700 rounded-lg px-3 py-2 text-sm focus:outline-none ${
            isAdmin
              ? "focus:ring-1 focus:ring-purple-500"
              : "cursor-default opacity-70 select-all"
          }`}
        />
      </div>

      {error && (
        <div className="rounded-xl border border-red-600/30 bg-red-500/10 px-4 py-3 text-sm text-red-400">
          {error}
        </div>
      )}

      {!agentId.trim() ? (
        <div className="rounded-xl border border-zinc-800 bg-zinc-900 px-5 py-12 text-center text-zinc-500 text-sm">
          Enter an Agent ID above to view its memory-lifecycle timeline.
        </div>
      ) : (
        <div className="space-y-4">
          {/* Header row: count + type legend + refresh */}
          <div className="flex items-center justify-between gap-3 flex-wrap">
            <p className="text-sm text-zinc-400">
              {loaded ? (
                <>
                  Showing{" "}
                  <span className="text-zinc-200 font-semibold">
                    {events.length}
                  </span>{" "}
                  of {total} event{total === 1 ? "" : "s"} — newest first
                </>
              ) : (
                "Loading lifecycle feed…"
              )}
            </p>
            <button
              onClick={() => load(0)}
              disabled={loading}
              className="flex items-center gap-1.5 px-3 py-2 text-sm rounded-lg border border-zinc-700 hover:bg-zinc-800 disabled:opacity-40 transition-colors"
            >
              <RefreshCw
                className={`w-3.5 h-3.5 ${loading ? "animate-spin" : ""}`}
              />
              Refresh
            </button>
          </div>

          {loaded && events.length > 0 && (
            <div className="flex flex-wrap gap-2">
              {Object.entries(counts).map(([type, n]) => {
                const s = styleFor(type);
                return (
                  <span
                    key={type}
                    className={`text-xs px-2 py-0.5 rounded-full border ${s.chip}`}
                  >
                    {s.label} · {n}
                  </span>
                );
              })}
            </div>
          )}

          {/* Body: loading / empty / timeline */}
          {loading && events.length === 0 ? (
            <div className="rounded-xl border border-zinc-800 bg-zinc-900 px-5 py-12 text-center text-zinc-500 text-sm animate-pulse">
              Loading lifecycle feed…
            </div>
          ) : loaded && events.length === 0 ? (
            <div className="rounded-xl border border-zinc-800 bg-zinc-900 px-5 py-12 text-center text-zinc-500 text-sm">
              No lifecycle events recorded yet for this agent. Ingest a memory or
              route a chat completion through the proxy to populate this
              timeline.
            </div>
          ) : (
            <ol className="relative border-l border-zinc-800 ml-2 space-y-3 pl-6">
              {events.map((e, i) => (
                <LifecycleRow key={`${e.timestamp}-${e.event_type}-${i}`} event={e} />
              ))}
            </ol>
          )}

          {hasMore && (
            <div className="flex justify-center">
              <button
                onClick={() => load(events.length)}
                disabled={loading}
                className="flex items-center gap-1.5 px-4 py-2 text-sm rounded-lg border border-zinc-700 hover:bg-zinc-800 disabled:opacity-40 transition-colors"
              >
                <History className={`w-3.5 h-3.5 ${loading ? "animate-pulse" : ""}`} />
                Load more ({total - events.length} remaining)
              </button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

// ── Single timeline row ─────────────────────────────────────────────────────────

function LifecycleRow({ event }: { event: LifecycleEvent }) {
  const s = styleFor(event.event_type);
  const Icon = s.icon;

  return (
    <li className="relative">
      <span
        className={`absolute -left-[31px] mt-1.5 w-2.5 h-2.5 rounded-full border ${s.dot}`}
      />
      <div className="rounded-xl border border-zinc-800 bg-zinc-900 px-4 py-3">
        <div className="flex items-start justify-between gap-3">
          <div className="flex-1 min-w-0">
            <div className="flex flex-wrap items-center gap-2">
              <span
                className={`inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded-full border ${s.chip}`}
              >
                <Icon className="w-3 h-3" />
                {s.label}
              </span>
              {event.memory_id && (
                <span className="font-mono text-[11px] text-zinc-600 truncate max-w-[220px]">
                  {event.memory_id}
                </span>
              )}
            </div>
            <div className="mt-2">
              <EventDetail event={event} />
            </div>
          </div>
          <span
            className="text-xs text-zinc-500 shrink-0 whitespace-nowrap"
            title={new Date(event.timestamp).toLocaleString()}
          >
            <Clock className="inline w-3 h-3 mr-1" />
            {relative(event.timestamp)}
          </span>
        </div>
      </div>
    </li>
  );
}

// ── Per-event-type detail rendering ─────────────────────────────────────────────
// detail shapes come straight from the jsonb_build_object() calls in
// src/memory/store.rs::fetch_lifecycle_events.

function EventDetail({ event }: { event: LifecycleEvent }) {
  const d = event.detail ?? {};

  if (
    event.event_type === "created" ||
    event.event_type === "modified" ||
    event.event_type === "status_changed" ||
    event.event_type === "narrative_generated"
  ) {
    const memoryType = str(d.memory_type);
    const status = str(d.status);
    const versionNumber = num(d.version_number);
    return (
      <div className="flex flex-wrap items-center gap-2 text-xs">
        {memoryType && <TypePill type={memoryType} />}
        {status && (
          <span className={STATUS_COLORS[status] ?? "text-zinc-400"}>
            {status}
          </span>
        )}
        {versionNumber !== null && (
          <span className="text-zinc-500">v{versionNumber}</span>
        )}
      </div>
    );
  }

  if (event.event_type === "consolidation" || event.event_type === "batch_restored") {
    const batchId = str(d.batch_id);
    const sourceCount = num(d.source_count);
    const l3Count = num(d.l3_count);
    const status = str(d.status);
    return (
      <div className="flex flex-wrap items-center gap-2 text-xs text-zinc-500">
        {sourceCount !== null && (
          <span>
            <span className="text-zinc-300">{sourceCount}</span> source
            {sourceCount === 1 ? "" : "s"}
          </span>
        )}
        {l3Count !== null && (
          <>
            <span className="text-zinc-600">→</span>
            <span>
              <span className="text-zinc-300">{l3Count}</span> L3
            </span>
          </>
        )}
        {status && (
          <span className={STATUS_COLORS[status] ?? "text-zinc-400"}>
            {status}
          </span>
        )}
        {batchId && (
          <span className="font-mono text-[11px] text-zinc-600 truncate max-w-[220px]">
            batch {batchId}
          </span>
        )}
      </div>
    );
  }

  if (event.event_type === "retrieval") {
    const injected = num(d.injected_count);
    const queryHash = str(d.query_hash);
    return (
      <div className="flex flex-wrap items-center gap-2 text-xs text-zinc-500">
        {injected !== null && (
          <span>
            <span className="text-zinc-300">{injected}</span> memor
            {injected === 1 ? "y" : "ies"} injected
          </span>
        )}
        {queryHash && (
          <span className="font-mono text-[11px] text-zinc-600 truncate max-w-[240px]">
            {queryHash}
          </span>
        )}
      </div>
    );
  }

  // Fallback: unknown event type — show raw detail so nothing is silently dropped.
  return (
    <pre className="text-[11px] text-zinc-500 font-mono whitespace-pre-wrap break-all">
      {JSON.stringify(d)}
    </pre>
  );
}

const TYPE_COLORS: Record<string, string> = {
  episodic: "bg-blue-500/20 text-blue-300 border-blue-500/30",
  semantic: "bg-green-500/20 text-green-300 border-green-500/30",
  procedural: "bg-yellow-500/20 text-yellow-300 border-yellow-500/30",
  narrative: "bg-purple-500/20 text-purple-300 border-purple-500/30",
};

function TypePill({ type }: { type: string }) {
  const cls = TYPE_COLORS[type] ?? "bg-zinc-700 text-zinc-300 border-zinc-600";
  return (
    <span className={`text-xs px-2 py-0.5 rounded-full border ${cls}`}>
      {type}
    </span>
  );
}
