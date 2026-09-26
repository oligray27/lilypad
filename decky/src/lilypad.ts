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
  /** How long the current session has run, in seconds, when the status was read. */
  now_tracking_secs: number | null;
  pending: number | null;
  new_games: number | null;
  decisions: number;
  storage_error: string | null;
  /** A newer LilyPad release, once the engine's daily check has found one. */
  update: Update | null;
}

export interface Update {
  version: string;
  /** The release page. */
  url: string;
  /** The plugin zip, for Decky's installer; null if the release has none. */
  zip_url: string | null;
  /** Its SHA-256 from the release's SHA256SUMS; null if not listed (installs unverified). */
  zip_sha256: string | null;
}

declare global {
  interface Window {
    /** Decky Loader's own backend connection (set up by the loader for its UI). */
    DeckyBackend?: { call: (route: string, ...args: unknown[]) => Promise<unknown> };
  }
}

/** Decky's `InstallType.UPDATE`: labels its confirmation prompt as an update. */
const DECKY_INSTALL_TYPE_UPDATE = 2;

/** Whether this Decky exposes the installer `installUpdate` uses. */
export const canInstallUpdates = () => typeof window.DeckyBackend?.call === "function";

/**
 * Hands the release's plugin zip to Decky's own installer -- the one behind its store and
 * "Install Plugin from URL". Decky shows its standard confirmation, downloads the zip, checks it
 * against `zip_sha256`, then replaces and reloads this plugin. Resolves once the prompt is up.
 * Not part of Decky's public plugin API (`utilities/install_plugin`, stable since Decky 3), so
 * callers keep the release page as a fallback.
 */
export async function installUpdate(update: Update): Promise<void> {
  if (!update.zip_url || !canInstallUpdates()) throw new Error("This Decky version can't install updates from LilyPad.");
  await window.DeckyBackend!.call(
    "utilities/install_plugin",
    update.zip_url,
    "LilyPad",
    update.version,
    update.zip_sha256 ?? "",
    DECKY_INSTALL_TYPE_UPDATE,
  );
}

export interface Decision {
  id: string;
  title: string;
  time: string;
  hours: number;
  /** Session-tracked and live-service games take notes, spoiler and visibility. */
  takes_notes: boolean;
  forced: boolean;
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
  /** Sent with every auto-submitted session; empty for none. */
  session_note: string;
  /** Off: the session dialog opens as each game closes instead. */
  auto_submit: boolean;
}

export type Attempt =
  | { outcome: "submitted" }
  | { outcome: "queued"; message: string }
  | { outcome: "failed"; message: string };

export type Retried =
  | { outcome: "submitted" }
  /** Its game was deleted from FrogLog, so it went to New Games under this title. */
  | { outcome: "moved_to_new_games"; title: string };

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
