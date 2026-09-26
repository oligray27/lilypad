// Full-screen dialogs for the parts that need more room than the Quick Access panel: a finished
// session to submit, the pending queue, and resolving New Games.

import { ConfirmModal, DialogButton, Dropdown, Field, Focusable, ModalRoot, TextField, ToggleField, showModal } from "@decky/ui";
import { useEffect, useState } from "react";
import {
  Attempt, Choice, Decision, IgdbResult, LibraryGame, NewGame, PendingSession, Retried, call, errorText,
} from "./lilypad";

interface ModalProps {
  closeModal?: () => void;
}

const rowStyle = { display: "flex", gap: "8px", flexWrap: "wrap" as const, marginTop: "6px" };
const errorStyle = { color: "#ff7b6b", marginTop: "8px" };

/** A finished session: submit it (with notes for session-tracked and live-service games) or
 * don't record it. Mirrors the desktop app's session window, wording included. Closing it
 * without choosing leaves the session in the panel's list. */
export function DecisionModal({ decision, closeModal }: ModalProps & { decision: Decision }) {
  const [notes, setNotes] = useState("");
  const [spoiler, setSpoiler] = useState(false);
  const [hidden, setHidden] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [done, setDone] = useState(false);

  const submit = async () => {
    setBusy(true);
    setMessage(null);
    try {
      const attempt = await call<Attempt>("decision_submit", { id: decision.id, notes, spoiler, is_public: !hidden });
      if (attempt.outcome === "submitted") return closeModal?.();
      setMessage(attempt.message);
      // Queued: saved for retry, nothing more to decide here.
      if (attempt.outcome === "queued") setDone(true);
    } catch (e) {
      setMessage(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const discard = async () => {
    setBusy(true);
    try {
      await call("decision_discard", { id: decision.id });
      closeModal?.();
    } catch (e) {
      setMessage(errorText(e));
      setBusy(false);
    }
  };

  return (
    <ModalRoot onCancel={closeModal}>
      {/* Named, since it opens over Steam's own UI with nothing else saying where it came from. */}
      <h2 style={{ margin: 0 }}>{decision.forced ? "LilyPad: Session Ended (Forced)" : "LilyPad: Session Ended"}</h2>
      <div style={{ opacity: 0.8, marginBottom: "12px" }}>{decision.title} – {decision.time}</div>
      {decision.takes_notes && !done && (
        <>
          <TextField label="Notes (optional)" value={notes} onChange={(e) => setNotes(e.target.value)} />
          <ToggleField label="Contains spoilers" checked={spoiler} onChange={setSpoiler} />
          <ToggleField label="Hide from public" checked={hidden} onChange={setHidden} />
        </>
      )}
      {message && <div style={done ? { marginTop: "8px" } : errorStyle}>{message}</div>}
      <Focusable style={rowStyle}>
        {done ? (
          <DialogButton onClick={closeModal}>Close</DialogButton>
        ) : (
          <>
            <DialogButton disabled={busy} onClick={submit}>Submit to FrogLog</DialogButton>
            <DialogButton disabled={busy} onClick={discard}>Do not record session</DialogButton>
          </>
        )}
      </Focusable>
    </ModalRoot>
  );
}

/** Sessions that have not reached FrogLog yet: retry or delete each one. */
export function PendingModal({ closeModal }: ModalProps) {
  const [rows, setRows] = useState<PendingSession[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<Record<string, string>>({});
  const [notice, setNotice] = useState<string | null>(null);

  const load = () =>
    call<PendingSession[]>("pending").then(setRows).catch((e) => setError(errorText(e)));
  useEffect(() => { load(); }, []);

  const act = async (row: PendingSession, cmd: "pending_retry" | "pending_delete", verb: string) => {
    setStatus((s) => ({ ...s, [row.id]: `${verb}…` }));
    try {
      const result = await call<Retried | null>(cmd, { id: row.id });
      if (result?.outcome === "moved_to_new_games") {
        setNotice(`${row.title} no longer exists in FrogLog, so its session was moved to New Games as ${result.title}.`);
      }
      await load();
    } catch (e) {
      setStatus((s) => ({ ...s, [row.id]: `${verb === "Submitting" ? "Retry" : "Delete"} failed: ${errorText(e)}` }));
    }
  };

  return (
    <ModalRoot onCancel={closeModal}>
      <h2 style={{ marginTop: 0 }}>Pending Submissions</h2>
      {notice && <div style={{ marginBottom: "8px" }}>{notice}</div>}
      {error && <div style={errorStyle}>{error}</div>}
      {rows === null && !error && <div>Loading…</div>}
      {rows?.length === 0 && <div>No pending submissions.</div>}
      {rows?.map((row) => (
        <Field
          key={row.id}
          label={row.title}
          description={
            <>
              <div>{row.hours}h · {row.date}</div>
              <div>{row.error}</div>
              {status[row.id] && <div>{status[row.id]}</div>}
            </>
          }
          childrenLayout="below"
        >
          <Focusable style={rowStyle}>
            <DialogButton onClick={() => act(row, "pending_retry", "Submitting")}>Retry</DialogButton>
            <DialogButton onClick={() => act(row, "pending_delete", "Deleting")}>Delete</DialogButton>
          </Focusable>
        </Field>
      ))}
      <Focusable style={rowStyle}>
        <DialogButton onClick={closeModal}>Close</DialogButton>
      </Focusable>
    </ModalRoot>
  );
}

/** Pick the FrogLog/IGDB title to create a New Games entry as. */
function CreateModal({ game, onResolved, closeModal }: ModalProps & { game: NewGame; onResolved: () => void }) {
  const [query, setQuery] = useState(game.title);
  const [results, setResults] = useState<IgdbResult[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const search = async () => {
    setBusy(true);
    setError(null);
    try {
      setResults(await call<IgdbResult[]>("igdb_search", { query }));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  useEffect(() => { search(); }, []);

  const create = async (title: string) => {
    setBusy(true);
    setError(null);
    try {
      await call("new_game_resolve", { appid: game.appid, choice: { kind: "new", igdb_title: title } as Choice });
      onResolved();
      closeModal?.();
    } catch (e) {
      setError(errorText(e));
      setBusy(false);
    }
  };

  return (
    <ModalRoot onCancel={closeModal}>
      <h2 style={{ marginTop: 0 }}>Add {game.title} to FrogLog</h2>
      <TextField label="Search" value={query} onChange={(e) => setQuery(e.target.value)} />
      <Focusable style={rowStyle}>
        <DialogButton disabled={busy} onClick={search}>Search</DialogButton>
      </Focusable>
      {error && <div style={errorStyle}>{error}</div>}
      {results?.length === 0 && <div style={{ marginTop: "8px" }}>No matches. Try a different search.</div>}
      <Focusable style={{ display: "flex", flexDirection: "column", gap: "6px", marginTop: "8px" }}>
        {results?.filter((r) => r.name).map((r, i) => (
          <DialogButton key={i} disabled={busy} onClick={() => create(r.name!)}>
            {r.name}{r.released ? ` (${r.released.slice(0, 4)})` : ""}
          </DialogButton>
        ))}
      </Focusable>
    </ModalRoot>
  );
}

/** "Map to Existing": log a New Games entry's sessions against a game the user already has.
 * Wording matches the desktop apps' New Games view. */
function ExistingModal({ game, onResolved, closeModal }: ModalProps & { game: NewGame; onResolved: () => void }) {
  const [library, setLibrary] = useState<LibraryGame[] | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    call<LibraryGame[]>("library").then(setLibrary).catch((e) => setError(errorText(e)));
  }, []);

  const confirm = async () => {
    if (selected === null || !library) return;
    const target = library[selected];
    setBusy(true);
    try {
      const choice: Choice = { kind: "existing", game_type: target.game_type, game_id: target.game_id, title: target.title };
      await call("new_game_resolve", { appid: game.appid, choice });
      onResolved();
      closeModal?.();
    } catch (e) {
      setError(errorText(e));
      setBusy(false);
    }
  };

  const label = (g: LibraryGame) => `${g.title}${g.game_type === "live" ? " (live service)" : g.status ? ` (${g.status})` : ""}`;

  return (
    <ModalRoot onCancel={closeModal}>
      <h2 style={{ marginTop: 0 }}>Map {game.title} to Existing</h2>
      {library === null && !error && <div>Loading…</div>}
      {library && (
        <Dropdown
          rgOptions={library.map((g, i) => ({ data: i, label: label(g) }))}
          selectedOption={selected}
          strDefaultLabel="Choose a game"
          onChange={(option) => setSelected(option.data as number)}
        />
      )}
      <div style={{ opacity: 0.8, fontSize: "0.85em", marginTop: "8px" }}>
        If the selected game is already marked Completed or DNF, logging hours here will clear its end date and return it to "In Progress" status.
      </div>
      {error && <div style={errorStyle}>{error}</div>}
      <Focusable style={rowStyle}>
        <DialogButton disabled={busy || selected === null} onClick={confirm}>Log Hours</DialogButton>
        <DialogButton onClick={closeModal}>Cancel</DialogButton>
      </Focusable>
    </ModalRoot>
  );
}

/** Games LilyPad saw being played that aren't in FrogLog yet (or are already finished there). */
export function NewGamesModal({ closeModal }: ModalProps) {
  const [games, setGames] = useState<NewGame[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = () => call<NewGame[]>("new_games").then(setGames).catch((e) => setError(errorText(e)));
  useEffect(() => { load(); }, []);

  const resolve = async (game: NewGame, choice: Choice) => {
    setError(null);
    try {
      await call("new_game_resolve", { appid: game.appid, choice });
      await load();
    } catch (e) {
      setError(errorText(e));
    }
  };

  const dismiss = (game: NewGame) =>
    showModal(
      <ConfirmModal
        strTitle={`Dismiss ${game.title}?`}
        strDescription={`${game.hours}h will not be logged to FrogLog.`}
        strOKButtonText="Dismiss"
        onOK={async () => {
          try {
            await call("new_game_dismiss", { appid: game.appid });
          } catch (e) {
            setError(errorText(e));
          }
          await load();
        }}
      />,
    );

  return (
    <ModalRoot onCancel={closeModal}>
      <h2 style={{ marginTop: 0 }}>New Games</h2>
      {error && <div style={errorStyle}>{error}</div>}
      {games === null && !error && <div>Loading…</div>}
      {games?.length === 0 && <div>No games detected outside FrogLog.</div>}
      {games?.map((game) => (
        <Field
          key={game.appid}
          label={game.title}
          description={
            game.replay_of
              ? `${game.hours}h · ${game.session_count} session(s). Already marked ${game.replay_of.status ?? "finished"} in FrogLog.`
              : `${game.hours}h · ${game.session_count} session(s)`
          }
          childrenLayout="below"
        >
          <Focusable style={rowStyle}>
            {game.replay_of ? (
              <>
                <DialogButton
                  onClick={() => resolve(game, {
                    kind: "existing", game_type: game.replay_of!.game_type, game_id: game.replay_of!.id, title: game.replay_of!.title,
                  })}
                >
                  Log to that entry
                </DialogButton>
                <DialogButton onClick={() => resolve(game, { kind: "replay" })}>New entry (replay)</DialogButton>
              </>
            ) : (
              <>
                <DialogButton onClick={() => showModal(<CreateModal game={game} onResolved={load} />)}>Add to FrogLog</DialogButton>
                <DialogButton onClick={() => showModal(<ExistingModal game={game} onResolved={load} />)}>Map to Existing</DialogButton>
              </>
            )}
            <DialogButton onClick={() => dismiss(game)}>Dismiss</DialogButton>
          </Focusable>
        </Field>
      ))}
      <Focusable style={rowStyle}>
        <DialogButton onClick={closeModal}>Close</DialogButton>
      </Focusable>
    </ModalRoot>
  );
}
