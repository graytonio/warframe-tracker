import { useEffect, useState } from 'react'

export type ItemSummary = { id: number; name: string; category: string; subcategory: string | null;
  is_prime: boolean; is_vaulted: boolean | null; image_url: string | null; wiki_url: string | null };

export type ProgressNode = { item: ItemSummary; needed: number; have: number; obtained: boolean;
  covered: boolean; children: ProgressNode[] };

export type Source = { kind: "mission" | "enemy" | "container" | "planet" | "relic"; name: string;
  rotation: string | null; chance: number | null; quantity: number | null;
  via: string | null;
  vaulted: boolean | null; rarity: string | null; pvp: boolean; planet: string | null };

export type WishlistEntry = { item: ItemSummary; added_at: string; completed_at: string | null;
  progress_pct: number; tree: ProgressNode };

export type InventoryRow = { item: ItemSummary; count: number; source: string; updated_at: string };

export type Need = { item: ItemSummary; needed: number; have: number; for: { id: number; name: string }[]; sources: Source[];
  location_hint: string | null };
export type SourceGroup = { kind: string; name: string; rotation: string | null; best_chance: number | null;
  vaulted: boolean; pvp: boolean; covers: { item_id: number; name: string; chance: number | null; via: string | null }[] };
export type LocationCover = { item_id: number; name: string; via: string[] };
export type Site = { site: string; kind: string; item_id: number; name: string;
  rotation: string | null; chance: number | null; quantity: number | null; via: string | null };
export type LocationGroup = { place: string; known: boolean; covers: LocationCover[]; nodes: string[]; sites: Site[] };
export type Plan = { by_need: Need[]; by_source: SourceGroup[]; by_location: LocationGroup[]; unsourced: Need[] };

export type Meta = { last_import_at: string | null; item_count: number;
  sources: { title: string; rev_id: number; fetched_at: string }[] };

export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    ...init,
    headers: init?.body ? { "content-type": "application/json", ...init?.headers } : init?.headers,
  });
  if (!res.ok) throw new Error((await res.json().catch(() => ({}))).error ?? res.statusText);
  return res.status === 204 ? (undefined as T) : res.json();
}

export const send = (method: string, path: string, body?: unknown) =>
  api<void>(path, { method, body: body === undefined ? undefined : JSON.stringify(body) });

export function useApi<T>(path: string): { data: T | undefined; error: string | undefined; reload: () => void } {
  const [data, setData] = useState<T>()
  const [error, setError] = useState<string>()
  const [tick, setTick] = useState(0)
  useEffect(() => {
    let live = true
    api<T>(path).then(
      d => { if (live) { setData(d); setError(undefined) } },
      e => { if (live) setError(String(e.message ?? e)) },
    )
    return () => { live = false }
  }, [path, tick])
  return { data, error, reload: () => setTick(t => t + 1) }
}

export const pct = (n: number | null) => (n == null ? '' : `${+n.toFixed(2)}%`)

export function describeSource(s: Source): string {
  return [
    `${s.name}${s.rotation ? ` (Rot ${s.rotation})` : ''}`,
    s.chance != null && pct(s.chance),
    s.quantity != null && s.quantity > 1 && `×${s.quantity}`,
    s.via && `via ${s.via}`,
    s.rarity,
    s.vaulted && 'vaulted',
  ].filter(Boolean).join(' · ')
}
