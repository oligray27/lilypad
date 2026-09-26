// LilyPad in the Quick Access Menu. The engine tracks in the background whether or not this panel
// is open; the panel shows what it is doing and handles everything that needs the user.

import { addEventListener, definePlugin, removeEventListener, toaster } from "@decky/api";
import {
  ButtonItem, ConfirmModal, Field, Navigation, PanelSection, PanelSectionRow, QuickAccessTab, TextField, ToggleField,
  showModal, staticClasses,
} from "@decky/ui";
import { useCallback, useEffect, useState } from "react";
import lilypadIcon from "../assets/lilypad.png";
import { DecisionModal, NewGamesModal, PendingModal } from "./modals";
import {
  Decision, EngineEvent, Settings, Status, Update, call, canInstallUpdates, errorText, installUpdate, publish, subscribe,
} from "./lilypad";

const openPanel = () => Navigation.OpenQuickAccessMenu(QuickAccessTab.Decky);

/** Toasts and dialogs for engine events, shown even while the panel is closed (i.e. mid-game). */
function toastFor(event: EngineEvent) {
  const s = (key: string) => String(event[key] ?? "");
  switch (event.event) {
    case "notify":
      toaster.toast({ title: s("summary"), body: s("body") });
      break;
    case "session_started":
      toaster.toast({ title: "Tracking Started", body: s("title") });
      break;
    // A session to submit (auto-submit off, or stopped from the panel): open the session dialog
    // straight away, as the desktop app opens its window. By the time a game's exit is noticed,
    // Steam's own UI is back in front.
    case "needs_decision":
      showModal(<DecisionModal decision={event.decision as Decision} />);
      break;
    // Only the first time this release is seen (shared with the desktop app); after that the
    // panel's update notice (from `status`) is the reminder.
    case "update_available":
      if (event.notify) {
        toaster.toast({ title: "New version available", body: "Open LilyPad to update.", onClick: openPanel });
      }
      break;
    case "new_game_recorded":
      toaster.toast({
        title: "Session Recorded",
        body: event.is_replay
          ? `${s("title")} (${s("time")}) is marked as finished in FrogLog. Open LilyPad to resolve it.`
          : `${s("title")} (${s("time")}) isn't in your FrogLog yet. Open LilyPad to add it.`,
        onClick: openPanel,
      });
      break;
  }
}

function LoginSection({ onDone }: { onDone: () => void }) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const login = async () => {
    setBusy(true);
    setError(null);
    try {
      await call("login", { username, password });
      setPassword("");
      onDone();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <PanelSection title="Log in to FrogLog">
      <PanelSectionRow>
        <TextField label="Username" value={username} onChange={(e) => setUsername(e.target.value)} />
      </PanelSectionRow>
      <PanelSectionRow>
        <TextField label="Password" bIsPassword value={password} onChange={(e) => setPassword(e.target.value)} />
      </PanelSectionRow>
      {error && <PanelSectionRow><Field description={error} /></PanelSectionRow>}
      <PanelSectionRow>
        <ButtonItem layout="below" disabled={busy || !username || !password} onClick={login}>
          {busy ? "Logging in…" : "Log in"}
        </ButtonItem>
      </PanelSectionRow>
    </PanelSection>
  );
}

/** The note sent with every auto-submitted session. Saved when the field loses focus, not on
 * each keystroke. */
function NoteField({ saved, onSaved }: { saved: string; onSaved: (settings: Settings) => void }) {
  const [note, setNote] = useState(saved);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => setNote(saved), [saved]);

  const save = async () => {
    if (note.trim() === saved) return;
    try {
      onSaved(await call<Settings>("settings_set", { session_note: note }));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  };

  return (
    <TextField
      label="Session message"
      description={error ?? "Sent with every auto-submitted session. Leave blank for none."}
      value={note}
      onChange={(e) => setNote(e.target.value)}
      onBlur={save}
    />
  );
}

function SettingsSection() {
  const [settings, setSettings] = useState<Settings | null>(null);
  useEffect(() => { call<Settings>("settings_get").then(setSettings).catch(() => undefined); }, []);
  if (!settings) return null;

  const set = (key: keyof Settings) => async (value: boolean) => {
    setSettings({ ...settings, [key]: value });
    try {
      setSettings(await call<Settings>("settings_set", { [key]: value }));
    } catch {
      setSettings(settings);
    }
  };

  return (
    <PanelSection title="Settings">
      <PanelSectionRow>
        <ToggleField
          label="Auto-submit sessions"
          description={settings.auto_submit ? undefined : "When a game closes, LilyPad asks you to submit the session, with notes."}
          checked={settings.auto_submit}
          onChange={set("auto_submit")}
        />
      </PanelSectionRow>
      {settings.auto_submit && (
        <PanelSectionRow>
          <NoteField saved={settings.session_note} onSaved={setSettings} />
        </PanelSectionRow>
      )}
      <PanelSectionRow>
        <ToggleField label="Mirror online presence to FrogLog" checked={settings.share_now_playing} onChange={set("share_now_playing")} />
      </PanelSectionRow>
      <PanelSectionRow>
        <ToggleField
          label="Record games not in FrogLog"
          description="They appear under New Games to add."
          checked={settings.detect_unmapped}
          onChange={set("detect_unmapped")}
        />
      </PanelSectionRow>
    </PanelSection>
  );
}

/** Shown at the top of the panel, in every state, once a newer release is out. "Update now"
 * hands the release's zip to Decky's installer (see `installUpdate`), which asks to confirm and
 * then replaces and reloads the plugin; the engine restarts with it, and a session in progress
 * is picked up again by its recovery. The release page stays as the fallback for a Decky
 * without that installer, or a release without a plugin zip. */
function UpdateNotice({ update }: { update: Update | null }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  if (!update) return null;

  const inPanel = Boolean(update.zip_url) && canInstallUpdates();
  const install = async () => {
    setBusy(true);
    setError(null);
    try {
      await installUpdate(update);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const openPage = () => {
    Navigation.CloseSideMenus();
    Navigation.NavigateToExternalWeb(update.url);
  };

  return (
    <PanelSection title="Update available">
      <PanelSectionRow>
        <Field
          label={`LilyPad ${update.version}`}
          description={error ?? (inPanel
            ? undefined
            : "Download the plugin zip from the release page, then install it with Decky's Install from ZIP.")}
        />
      </PanelSectionRow>
      {inPanel && (
        <PanelSectionRow>
          <ButtonItem layout="below" disabled={busy} onClick={install}>{busy ? "Starting update…" : "Update now"}</ButtonItem>
        </PanelSectionRow>
      )}
      <PanelSectionRow><ButtonItem layout="below" onClick={openPage}>Open release page</ButtonItem></PanelSectionRow>
    </PanelSection>
  );
}

/** HH:MM:SS, e.g. 01:02:03. */
function clock(totalSecs: number): string {
  const s = Math.max(0, Math.floor(totalSecs));
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(Math.floor(s / 3600))}:${pad(Math.floor((s % 3600) / 60))}:${pad(s % 60)}`;
}

/** The current game and a session clock that ticks locally from the engine's last reading. */
function NowTracking({ title, secs }: { title: string; secs: number }) {
  const [startedAt, setStartedAt] = useState(() => Date.now() - secs * 1000);
  const [now, setNow] = useState(Date.now());
  useEffect(() => setStartedAt(Date.now() - secs * 1000), [title, secs]);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  return <Field label={`Now Tracking: ${title}`} description={`${clock((now - startedAt) / 1000)} - this session`} />;
}

function Content() {
  const [status, setStatus] = useState<Status | null>(null);
  const [decisions, setDecisions] = useState<Decision[]>([]);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const next = await call<Status>("status");
      setStatus(next);
      setError(null);
      setDecisions(next.tracking && next.logged_in ? await call<Decision[]>("decisions") : []);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);

  useEffect(() => {
    refresh();
    return subscribe(() => { refresh(); });
  }, [refresh]);

  if (!status) {
    return (
      <PanelSection>
        <PanelSectionRow><Field label={error ? "LilyPad isn't running" : "Starting LilyPad…"} description={error ?? undefined} /></PanelSectionRow>
      </PanelSection>
    );
  }

  const updateNotice = <UpdateNotice update={status.update} />;

  if (!status.tracking) {
    return (
      <>
        {updateNotice}
        <PanelSection>
          <PanelSectionRow>
            <Field
              label="Tracking from the desktop app"
              description="LilyPad is running in Desktop Mode, so it is tracking there. Gaming Mode takes over when you switch back."
            />
          </PanelSectionRow>
        </PanelSection>
      </>
    );
  }

  if (!status.logged_in) return <>{updateNotice}<LoginSection onDone={refresh} /></>;

  const stopTracking = () =>
    showModal(
      <ConfirmModal
        strTitle="Stop tracking this session?"
        strDescription="Use this if LilyPad picked the wrong game. You can still submit the time or not record it."
        strOKButtonText="Stop tracking"
        onOK={() => { call("force_stop").then(refresh).catch(() => undefined); }}
      />,
    );

  const logout = () =>
    showModal(
      <ConfirmModal
        strTitle="Log out of FrogLog?"
        strOKButtonText="Log out"
        onOK={() => { call("logout").then(refresh).catch(() => undefined); }}
      />,
    );

  return (
    <>
      {updateNotice}
      {status.storage_error && (
        <PanelSection><PanelSectionRow><Field label="Storage problem" description={status.storage_error} /></PanelSectionRow></PanelSection>
      )}
      <PanelSection>
        <PanelSectionRow>
          {status.now_tracking
            ? <NowTracking title={status.now_tracking} secs={status.now_tracking_secs ?? 0} />
            : <Field label="Not tracking a game" />}
        </PanelSectionRow>
        {status.now_tracking && (
          <PanelSectionRow><ButtonItem layout="below" onClick={stopTracking}>Stop tracking this session</ButtonItem></PanelSectionRow>
        )}
      </PanelSection>

      {decisions.length > 0 && (
        <PanelSection title="Sessions to submit">
          {decisions.map((d) => (
            <PanelSectionRow key={d.id}>
              <ButtonItem layout="below" label={`${d.title} · ${d.time}`} onClick={() => showModal(<DecisionModal decision={d} />)}>
                Submit or don't record
              </ButtonItem>
            </PanelSectionRow>
          ))}
        </PanelSection>
      )}

      <PanelSection title="Queues">
        <PanelSectionRow>
          <ButtonItem layout="below" onClick={() => showModal(<PendingModal />)}>
            Pending Submissions{status.pending ? ` (${status.pending})` : ""}
          </ButtonItem>
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem layout="below" onClick={() => showModal(<NewGamesModal />)}>
            New Games{status.new_games ? ` (${status.new_games})` : ""}
          </ButtonItem>
        </PanelSectionRow>
      </PanelSection>

      <SettingsSection />

      <PanelSection title="Account">
        <PanelSectionRow><Field label={status.username ?? "Logged in"} description={`LilyPad ${status.version}`} /></PanelSectionRow>
        <PanelSectionRow><ButtonItem layout="below" onClick={logout}>Log out</ButtonItem></PanelSectionRow>
      </PanelSection>
    </>
  );
}

export default definePlugin(() => {
  const listener = addEventListener<[EngineEvent]>("lilypad_event", (event) => {
    toastFor(event);
    publish(event);
  });
  return {
    name: "LilyPad",
    titleView: <div className={staticClasses.Title}>LilyPad</div>,
    content: <Content />,
    icon: <img src={lilypadIcon} alt="" style={{ width: "1em", height: "1em" }} />,
    onDismount() {
      removeEventListener("lilypad_event", listener);
    },
  };
});
