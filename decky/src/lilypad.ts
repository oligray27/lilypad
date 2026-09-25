// The panel's view of the LilyPad engine: typed requests through the plugin backend's `call`,
// and a small bus for the engine's events so every open view can refresh when something changes.

import { callable } from "@decky/api";

export interface Status {
  version: string;
  /** False while the desktop app is tracking (it has priority); queues belong to it then. */
  tracking: boolean;
  logged_in: boolean;
  username: string | null;
  now_tracking: string | null;
  pending: number | null;
  new_games: number | null;
  decisions: number;
  storage_error: string | null;
}

export interface Decision {
  id: string;
  title: string;
  time: string;
  hours: number;
}

export interface PendingSession {
  id: string;
  title: string;
  hours: number;
  date: string;
  notes: string | null;
  error: string;
}

export interface ReplayOf {
  id: number;
  game_type: string;
  title: string;
  status: string | null;
}

export interface NewGame {
  appid: string;
  title: string;
  hours: number;
  session_count: number;
  replay_of: ReplayOf | null;
}

export interface LibraryGame {
  game_type: string;
  game_id: number;
  title: string;
  status: string | null;
}

export interface IgdbResult {
  name?: string;
  released?: string;
}

export interface Settings {
  share_now_playing: boolean;
  detect_unmapped: boolean;
  /** Sent with every session; empty for none. */
  session_note: string;
}

export type Attempt =
  | { outcome: "submitted" }
  | { outcome: "queued"; message: string }
  | { outcome: "failed"; message: string };

export type Choice =
  | { kind: "new"; igdb_title: string }
  | { kind: "replay" }
  | { kind: "existing"; game_type: string; game_id: number; title: string };

export interface EngineEvent {
  event: string;
  [key: string]: unknown;
}

interface Reply {
  ok: boolean;
  result?: unknown;
  error?: string;
}

const rawCall = callable<[cmd: string, args: object], Reply>("call");

/** One engine request. Throws the engine's own explanation on failure. */
export async function call<T>(cmd: string, args: object = {}): Promise<T> {
  const reply = await rawCall(cmd, args);
  if (!reply.ok) throw new Error(reply.error ?? "LilyPad could not do that.");
  return reply.result as T;
}

type Listener = (event: EngineEvent) => void;
const listeners = new Set<Listener>();

export function publish(event: EngineEvent) {
  listeners.forEach((listener) => listener(event));
}

/** Subscribes to engine events; returns the unsubscribe function. */
export function subscribe(listener: Listener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
