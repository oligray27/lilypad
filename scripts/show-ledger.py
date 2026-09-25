#!/usr/bin/env python3
"""Prints LilyPad's session store, read-only: one line per record, newest last.

    python3 scripts/show-ledger.py            # Linux default location
    python3 scripts/show-ledger.py PATH       # any sessions.sqlite (e.g. Windows %APPDATA%)

Healthy after a test run: nothing left `active` (unless a game is running now), and `pending`
only for sessions deliberately left unsubmitted. Two records with the same start time and
game are a duplicate. Opens the database read-only, so it is safe while LilyPad runs.
"""
import datetime
import json
import os
import sqlite3
import sys

path = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/.local/share/froglog-lilypad/sessions.sqlite")
db = sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def when(secs):
    return datetime.datetime.fromtimestamp(secs).strftime("%m-%d %H:%M:%S") if secs else "-"


counts = {}
for rid, raw, state, remote in db.execute("SELECT id, record, state, remote_id FROM sessions ORDER BY rowid"):
    r = json.loads(raw)
    kind, data = r["target"]["kind"], r["target"]["data"]
    name = data.get("title") or data.get("process") or "?"
    start, end = r.get("started_at_secs"), r.get("ended_at_secs")
    length = f"{end - start}s" if start and end else "-"
    owner = "yes" if r.get("account") else "NONE"
    error = (r.get("submission") or {}).get("last_error") or ""
    counts[state] = counts.get(state, 0) + 1
    print(f"{when(start)}  {state:12} {kind:15} {name[:26]:26} {length:>7}  owner={owner:4} "
          f"id={rid[:8]} remote={remote or '-'} {error[:50]}")
print("\n" + ", ".join(f"{n} {state}" for state, n in sorted(counts.items())))
