// Use global Tauri API (no bundler – script runs as plain module from serve)
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

// No devtools feature is compiled in (see src-tauri/Cargo.toml), so this just
// suppresses WebView2's default context menu (Back/Forward/Reload/etc).
document.addEventListener('contextmenu', (e) => e.preventDefault());

const VIEW_SIZE = {
  loginView: { width: 560, height: 318 },
  mainView: { width: 550, height: 335 },
  mappingsView: { width: 642, height: 760 },
  sessionView: { width: 440, height: 165 },
  pendingView: { width: 550, height: 480 },
};

function showView(id, heightOrOpts) {
  document.querySelectorAll('[data-view]').forEach((el) => {
    el.hidden = el.id !== id;
  });
  const spec = VIEW_SIZE[id] || { width: 642, height: 625 };
  let w = spec.width;
  let h = spec.height;
  if (heightOrOpts != null) {
    if (typeof heightOrOpts === 'number') {
      h = heightOrOpts;
    } else {
      if (heightOrOpts.width != null) w = heightOrOpts.width;
      if (heightOrOpts.height != null) h = heightOrOpts.height;
    }
  }
  invoke('set_window_size', { width: w, height: h }).catch(() => {});
}

function formatDuration(secs) {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (h > 0) return `${h}h ${m}m`;
  return `${m} min`;
}

// Login form
async function onLogin(e) {
  e.preventDefault();
  const baseUrl = 'https://api.froglog.co.uk/api';
  const username = $('username').value.trim();
  const password = $('password').value;
  const rememberMe = $('rememberMe').checked;
  const errEl = $('loginError');
  errEl.textContent = '';
  try {
    await invoke('login', { baseUrl, username, password, rememberMe });
    showView('mainView');
    loadMainView();
    // Refresh tray menu after a delay so it runs when main thread is idle (avoids lockup).
    setTimeout(() => {
      invoke('refresh_tray_menu').catch(() => {});
    }, 1500);
  } catch (err) {
    errEl.textContent = String(err);
  }
}

// Main view (about screen) — shows pending submissions notice if any exist
async function loadMainView() {
  const sessions = await invoke('get_pending_sessions').catch(() => []);
  const notice = $('pendingNotice');
  if (!notice) return;
  notice.hidden = false;
  if (sessions.length) {
    notice.style.color = 'darkorange';
    notice.innerHTML = `&#9888; ${sessions.length} pending submission${sessions.length > 1 ? 's' : ''} — <a href="#" id="pendingNoticeLink">View</a>`;
    const link = $('pendingNoticeLink');
    if (link) {
      link.addEventListener('click', (e) => {
        e.preventDefault();
        showView('pendingView');
        loadPendingView();
      });
    }
  } else {
    notice.innerHTML = '&#10003; No pending submissions';
    notice.style.color = '';
  }
}

async function loadPendingView() {
  const list = $('pendingList');
  if (!list) return;
  list.innerHTML = '<p class="muted" style="padding:1rem;">Loading…</p>';
  const sessions = await invoke('get_pending_sessions').catch(() => []);
  if (!sessions.length) {
    list.innerHTML = '<p class="muted" style="padding:1rem;">No pending submissions.</p>';
    return;
  }
  list.innerHTML = sessions.map((s) => `
    <div class="pending-item" data-id="${escapeAttr(s.id)}">
      <div class="pending-title">${escapeHtml(s.title || `${s.game_type} #${s.game_id}`)}</div>
      <div class="pending-meta">
        <span><strong>Session Length:</strong> ${s.hours}h</span>
        <span><strong>Session Date:</strong> ${s.date}</span>
        ${s.notes ? `<span><strong>Session Notes:</strong> ${escapeHtml(s.notes)}</span>` : ''}
      </div>
      <div class="pending-actions">
        <button type="button" class="pending-retry">Retry</button>
        <button type="button" class="pending-delete">Delete</button>
        <span class="pending-status"></span>
      </div>
    </div>
  `).join('');

  list.querySelectorAll('.pending-retry').forEach((btn) => {
    btn.addEventListener('click', async () => {
      const item = btn.closest('[data-id]');
      const id = item.dataset.id;
      const status = item.querySelector('.pending-status');
      btn.disabled = true;
      status.textContent = 'Submitting…';
      status.style.color = '';
      try {
        await invoke('retry_pending_session', { id });
        item.remove();
        invoke('refresh_tray_menu').catch(() => {});
        if (!$('pendingList').querySelector('.pending-item')) {
          $('pendingList').innerHTML = '<p class="muted" style="padding:1rem;">No pending submissions.</p>';
        }
      } catch (err) {
        btn.disabled = false;
        status.textContent = String(err);
        status.style.color = 'red';
      }
    });
  });

  list.querySelectorAll('.pending-delete').forEach((btn) => {
    btn.addEventListener('click', async () => {
      const item = btn.closest('[data-id]');
      await invoke('delete_pending_session', { id: item.dataset.id }).catch(() => {});
      item.remove();
      invoke('refresh_tray_menu').catch(() => {});
      if (!$('pendingList').querySelector('.pending-item')) {
        $('pendingList').innerHTML = '<p class="muted" style="padding:1rem;">No pending submissions.</p>';
      }
    });
  });
}

function loadVersion() {
  window.__TAURI__.app.getVersion().then((v) => {
    document.querySelectorAll('.app-version').forEach((el) => {
      el.innerHTML = `v${v} <a href="#" class="ext-link" data-url="https://github.com/oligray27/lilypad/releases/latest" title="View releases">(?)</a>`;
    });
  }).catch(() => {});
}

// --- Mappings view: table of games with exe column ---
let mappingsAllRows = [];
let pendingExeFor = {};        // working copy (not yet saved to disk)
let pendingTitleFilterFor = {};
let savedExeFor = {};          // reflects what's currently on disk
let savedTitleFilterFor = {};
let mappingsMode = 'regular'; // 'regular' | 'live' | 'session'
let mappingsPage = 0;
let mappingsSearch = '';
const MAPPINGS_PAGE_SIZE = 6;

function mappingKey(type, id) {
  return `${type}:${id}`;
}
function exeByGame(mappings) {
  const out = {};
  (mappings || []).forEach((m) => {
    out[mappingKey(m.type, m.froglog_id)] = m.process;
  });
  return out;
}
function titleFilterByGame(mappings) {
  const out = {};
  (mappings || []).forEach((m) => {
    out[mappingKey(m.type, m.froglog_id)] = m.title_filter || '';
  });
  return out;
}

function renderMappingsTable() {
  const tableBody = $('mappingsTableBody');
  if (!tableBody || !mappingsAllRows.length) return;
  const needle = mappingsSearch.toLowerCase();
  const filtered = mappingsAllRows.filter((r) => r.type === mappingsMode && (!needle || r.title.toLowerCase().includes(needle)));
  const totalPages = Math.max(1, Math.ceil(filtered.length / MAPPINGS_PAGE_SIZE));
  mappingsPage = Math.min(Math.max(0, mappingsPage), totalPages - 1);
  const rows = filtered.slice(mappingsPage * MAPPINGS_PAGE_SIZE, (mappingsPage + 1) * MAPPINGS_PAGE_SIZE);
  tableBody.innerHTML = rows
    .map((row) => {
      const key = mappingKey(row.type, row.id);
      const exe = pendingExeFor[key] || '';
      const titleFilter = pendingTitleFilterFor[key] || '';
      const titleEsc = escapeHtml(row.title);
      const exeEsc = escapeAttr(exe);
      const tfEsc = escapeAttr(titleFilter);
      return `<tr data-type="${escapeAttr(row.type)}" data-id="${row.id}" data-title="${escapeAttr(row.title)}">
        <td class="mappings-title">${titleEsc}</td>
        <td class="mappings-exe-cell"><div class="mappings-exe-cell-inner"><input type="text" class="mappings-exe" value="${exeEsc}" placeholder="e.g. hl2.exe" data-type="${escapeAttr(row.type)}" data-id="${row.id}" data-title="${escapeAttr(row.title)}" /><button type="button" class="mappings-browse">Browse…</button></div></td>
        <td class="mappings-title-filter-cell"><input type="text" class="mappings-title-filter" value="${tfEsc}" placeholder="optional" data-type="${escapeAttr(row.type)}" data-id="${row.id}" data-title="${escapeAttr(row.title)}" /></td>
      </tr>`;
    })
    .join('');
  tableBody.querySelectorAll('.mappings-exe').forEach((input) => {
    input.addEventListener('blur', onExeBlur);
  });
  tableBody.querySelectorAll('.mappings-browse').forEach((btn) => {
    btn.addEventListener('click', onExeBrowse);
  });
  tableBody.querySelectorAll('.mappings-title-filter').forEach((input) => {
    input.addEventListener('blur', onTitleFilterBlur);
  });
  const btnGames = $('mappingsSwitchGames');
  const btnLive = $('mappingsSwitchLive');
  const btnSession = $('mappingsSwitchSession');
  if (btnGames) btnGames.classList.toggle('active', mappingsMode === 'regular');
  if (btnLive) btnLive.classList.toggle('active', mappingsMode === 'live');
  if (btnSession) btnSession.classList.toggle('active', mappingsMode === 'session');
  const paginationEl = $('mappingsPagination');
  const prevBtn = $('mappingsPrev');
  const nextBtn = $('mappingsNext');
  const pageInfo = $('mappingsPageInfo');
  if (paginationEl) paginationEl.hidden = totalPages <= 1;
  if (prevBtn) prevBtn.disabled = mappingsPage === 0;
  if (nextBtn) nextBtn.disabled = mappingsPage >= totalPages - 1;
  if (pageInfo) pageInfo.textContent = `${mappingsPage + 1} / ${totalPages}`;
  refreshMappingsState();
}

async function loadMappingsView() {
  const errEl = $('mappingsError');
  const tableWrap = $('mappingsTableWrap');
  const tableBody = $('mappingsTableBody');
  if (!errEl || !tableWrap || !tableBody) return;
  errEl.textContent = '';
  tableBody.innerHTML = '';
  tableWrap.hidden = true;
  // Load checkbox states independently — get_process_mappings needs no auth so this
  // always reflects the correct state even when the game list fetch fails
  invoke('get_process_mappings').then((mc) => {
    const asRegEl = $('mappingsAutoSubmitRegular');
    if (asRegEl) asRegEl.checked = !!mc.auto_submit_regular;
    const asLiveEl = $('mappingsAutoSubmitLive');
    if (asLiveEl) asLiveEl.checked = !!mc.auto_submit_live;
    const asSessionEl = $('mappingsAutoSubmitSession');
    if (asSessionEl) asSessionEl.checked = !!mc.auto_submit_session;
    const shareNowEl = $('shareNowPlaying');
    if (shareNowEl) shareNowEl.checked = !!mc.share_now_playing;
  }).catch(() => {});
  // Pending notice — runs independently so it always shows even if game data fetch fails
  invoke('get_pending_sessions').catch(() => []).then((pendingSessions) => {
    const mNotice = $('mappingsPendingNotice');
    if (!mNotice) return;
    mNotice.hidden = false;
    if (pendingSessions && pendingSessions.length) {
      mNotice.style.color = 'darkorange';
      mNotice.innerHTML = `&#9888; ${pendingSessions.length} pending submission${pendingSessions.length > 1 ? 's' : ''} — <a href="#" id="mappingsPendingLink">View</a>`;
      const link = $('mappingsPendingLink');
      if (link) link.addEventListener('click', (e) => { e.preventDefault(); showView('pendingView'); loadPendingView(); });
    } else {
      mNotice.style.color = '';
      mNotice.innerHTML = '&#10003; No pending submissions';
    }
  });
  try {
    const [games, liveService, mapConfig] = await Promise.all([
      invoke('get_games'),
      invoke('get_live_service_games'),
      invoke('get_process_mappings'),
    ]);
    console.log('[LilyPad] loadMappingsView mapConfig:', mapConfig);
    const knownKeys = new Set([
      ...(games || []).map((g) => mappingKey(g.session_tracking ? 'session' : 'regular', g.id)),
      ...(liveService || []).map((g) => mappingKey('live', g.id)),
    ]);
    const toRemove = (mapConfig.mappings || []).filter((m) => !knownKeys.has(mappingKey(m.type, m.froglog_id)));
    for (const m of toRemove) {
      await invoke('delete_process_mapping', { froglogId: m.froglog_id, gameType: m.type });
    }
    const mapConfigAfter = toRemove.length ? await invoke('get_process_mappings') : mapConfig;
    const loadedExe = exeByGame(mapConfigAfter.mappings || []);
    const loadedTitleFilter = titleFilterByGame(mapConfigAfter.mappings || []);
    pendingExeFor = { ...loadedExe };
    pendingTitleFilterFor = { ...loadedTitleFilter };
    savedExeFor = { ...loadedExe };
    savedTitleFilterFor = { ...loadedTitleFilter };
    mappingsAllRows = [
      ...(games || []).map((g) => ({ id: g.id, title: g.title || `#${g.id}`, type: g.session_tracking ? 'session' : 'regular' })),
      ...(liveService || []).map((g) => ({ id: g.id, title: g.title || `#${g.id}`, type: 'live' })),
    ];
    mappingsMode = 'regular';
    mappingsPage = 0;
    mappingsSearch = '';
    const searchEl = $('mappingsSearch');
    if (searchEl) searchEl.value = '';
    const asRegEl = $('mappingsAutoSubmitRegular');
    if (asRegEl) asRegEl.checked = !!mapConfigAfter.auto_submit_regular;
    const asLiveEl = $('mappingsAutoSubmitLive');
    if (asLiveEl) asLiveEl.checked = !!mapConfigAfter.auto_submit_live;
    const asSessionEl = $('mappingsAutoSubmitSession');
    if (asSessionEl) asSessionEl.checked = !!mapConfigAfter.auto_submit_session;
    const shareNowEl = $('shareNowPlaying');
    if (shareNowEl) {
      shareNowEl.checked = !!mapConfigAfter.share_now_playing;
      console.log('[LilyPad] share_now_playing loaded as:', shareNowEl.checked);
    }
    tableWrap.hidden = false;
    renderMappingsTable();
  } catch (e) {
    errEl.textContent = 'Could not load data. ' + (e && e.message ? e.message : String(e)) + ' Log in via Settings if needed.';
  }
}

function setMappingsMode(mode) {
  mappingsMode = mode;
  mappingsPage = 0;
  mappingsSearch = '';
  const searchEl = $('mappingsSearch');
  if (searchEl) searchEl.value = '';
  renderMappingsTable();
}

function escapeHtml(s) {
  const div = document.createElement('div');
  div.textContent = s == null ? '' : s;
  return div.innerHTML;
}
function escapeAttr(s) {
  return escapeHtml(s == null ? '' : s).replace(/"/g, '&quot;');
}

function norm(v) {
  return (v || '').trim();
}

/** Returns a Set of mapping keys that conflict with each other.
Conflict = two different entries sharing the same exe where:
- Both have no title filter, OR
- Both have the same (non-empty) title filter, OR
- One has no filter when multiple games share the exe */
function detectConflicts() {
    const conflicts = new Set();
    const byExe = {};
    
    for (const [key, exe] of Object.entries(pendingExeFor)) {
        if (!exe) continue;
        const exeLower = exe.toLowerCase();
        if (!byExe[exeLower]) byExe[exeLower] = [];
        byExe[exeLower].push({ 
            key, 
            titleFilter: norm(pendingTitleFilterFor[key]).toLowerCase() 
        });
    }
    
    for (const [exeLower, entries] of Object.entries(byExe)) {
        if (entries.length < 2) continue;
        
        // Check if any entry is missing a filter when there are multiple games
        const hasMissingFilter = entries.some(e => !e.titleFilter);
        
        for (let i = 0; i < entries.length; i++) {
            for (let j = i + 1; j < entries.length; j++) {
                const a = entries[i], b = entries[j];
                const aBare = !a.titleFilter, bBare = !b.titleFilter;
                const sameFilter = a.titleFilter && b.titleFilter && a.titleFilter === b.titleFilter;
                
                // Conflict if: both missing filters, same filter, OR missing filter when multiple exist
                if ((aBare && bBare) || sameFilter || (hasMissingFilter && entries.length > 1)) {
                    conflicts.add(a.key);
                    conflicts.add(b.key);
                }
            }
        }
    }
    
    return conflicts;
}

/** Update conflict highlights, Apply button enabled state, and error text. */
function refreshMappingsState() {
    const conflicts = detectConflicts();
    
    // Dirty check: any key where pending differs from saved
    const allKeys = new Set([
        ...Object.keys(pendingExeFor),
        ...Object.keys(savedExeFor),
        ...Object.keys(pendingTitleFilterFor),
        ...Object.keys(savedTitleFilterFor),
    ]);
    let hasDirty = false;
    for (const k of allKeys) {
        if (norm(pendingExeFor[k]) !== norm(savedExeFor[k]) ||
            norm(pendingTitleFilterFor[k]) !== norm(savedTitleFilterFor[k])) {
            hasDirty = true;
            break;
        }
    }

    // Highlight conflicting exe and title-filter inputs in visible rows
    document.querySelectorAll('.mappings-exe, .mappings-title-filter').forEach((input) => {
        const key = mappingKey(input.dataset.type, input.dataset.id);
        input.classList.toggle('mappings-exe-conflict', conflicts.has(key));
    });
    
    // Highlight mode switch buttons whose tab contains conflicts
    const conflictTypes = new Set([...conflicts].map((k) => k.split(':')[0]));
    const switchBtnMap = { 
        regular: 'mappingsSwitchGames', 
        session: 'mappingsSwitchSession', 
        live: 'mappingsSwitchLive' 
    };
    for (const [type, btnId] of Object.entries(switchBtnMap)) {
        const btn = $(btnId);
        if (btn) btn.classList.toggle('mappings-switch-conflict', conflictTypes.has(type));
    }
    
    // Apply button state
    const applyBtn = $('mappingsApply');
    if (applyBtn) {
        applyBtn.disabled = conflicts.size > 0 || !hasDirty;
    }
    
    // Error/conflict message
    const errEl = $('mappingsError');
    if (errEl) {
        if (conflicts.size > 0) {
            // Check what type of conflict
            const byExe = {};
            for (const [key, exe] of Object.entries(pendingExeFor)) {
                if (!exe) continue;
                const exeLower = exe.toLowerCase();
                if (!byExe[exeLower]) byExe[exeLower] = [];
                byExe[exeLower].push({ 
                    key, 
                    titleFilter: norm(pendingTitleFilterFor[key]) 
                });
            }
            
            const hasMissingFilter = Object.values(byExe).some(entries => 
                entries.length > 1 && entries.some(e => !e.titleFilter)
            );
            
            if (hasMissingFilter) {
                errEl.textContent = 'A window title filter is required when multiple games share the same executable. Add unique filters to distinguish them.';
            } else {
                errEl.textContent = 'Conflicted mappings highlighted in red. Each mapping must have a unique executable or title filter combination.';
            }
        } else if (errEl.textContent.includes('Conflicted') || errEl.textContent.includes('Window Title Filter')) {
            errEl.textContent = '';
        }
    }
}

async function onExeBrowse(e) {
  const btn = e.target;
  const cell = btn.closest('.mappings-exe-cell');
  const input = cell && cell.querySelector('.mappings-exe');
  if (!input) return;
  try {
    const path = await invoke('pick_exe_file');
    if (path && typeof path === 'string') {
      const name = path.replace(/^.*[/\\]/, '');
      if (name) {
        input.value = name;
        input.dispatchEvent(new Event('blur', { bubbles: true }));
      }
    }
  } catch (err) {
    const errEl = $('mappingsError');
    if (errEl) errEl.textContent = String(err);
  }
}

function onExeBlur(e) {
  const input = e.target;
  const gameType = input.dataset.type;
  const gameId = parseInt(input.dataset.id, 10);
  const key = mappingKey(gameType, gameId);
  pendingExeFor[key] = input.value.trim();
  refreshMappingsState();
}

function onTitleFilterBlur(e) {
  const input = e.target;
  const gameType = input.dataset.type;
  const gameId = parseInt(input.dataset.id, 10);
  const key = mappingKey(gameType, gameId);
  pendingTitleFilterFor[key] = input.value.trim();
  refreshMappingsState();
}

async function doApply() {
    const errEl = $('mappingsError');
    if (errEl) errEl.textContent = '';
    
    const allKeys = new Set([
        ...Object.keys(pendingExeFor),
        ...Object.keys(savedExeFor),
    ]);
    
    for (const key of allKeys) {
        const [gameType, idStr] = key.split(':');
        const gameId = parseInt(idStr, 10);
        const newExe = norm(pendingExeFor[key]);
        const oldExe = norm(savedExeFor[key]);
        const newFilter = norm(pendingTitleFilterFor[key]) || null;
        const oldFilter = norm(savedTitleFilterFor[key]) || null;
        
        if (newExe === oldExe && newFilter === oldFilter) continue;
        
        try {
            if (newExe) {
                const row = mappingsAllRows.find((r) => mappingKey(r.type, r.id) === key);
                await invoke('save_process_mapping', {
                    process: newExe,
                    gameType,
                    froglogId: gameId,
                    title: (row && row.title) || undefined,
                    titleFilter: newFilter || undefined,
                });
            } else {
                await invoke('delete_process_mapping', { froglogId: gameId, gameType });
            }
            savedExeFor[key] = newExe;
            savedTitleFilterFor[key] = newFilter || '';
        } catch (err) {
            if (errEl) {
                // Provide more specific error messages
                if (err.includes('Window Title Filter is required')) {
                    errEl.textContent = 'Filter Required: ' + err;
                } else if (err.includes('Multiple games are mapped')) {
                    errEl.textContent = 'Configuration Error: ' + err;
                } else {
                    errEl.textContent = String(err);
                }
            }
            return;
        }
    }
    
    refreshMappingsState();
    
    // If successful, show success message briefly
    if (errEl && !errEl.textContent) {
        errEl.style.color = 'green';
        errEl.textContent = 'Mappings saved successfully!';
        setTimeout(() => {
            errEl.textContent = '';
            errEl.style.color = '';
        }, 3000);
    }
}
// Post-play popup (shown when session-ended fires)
let pendingSession = null;

/** Hours for FrogLog: 2 decimal places, minimum 0.01. */
function roundHoursForFroglog(hours) {
  const rounded = Math.round(hours * 100) / 100;
  return rounded < 0.01 ? 0.01 : rounded;
}

function showPostPlay(data) {
  pendingSession = data;
  const mapping = data.mapping || {};
  const title = mapping.title || `Game #${mapping.froglogId || ''}`;
  $('sessionHeader').textContent = data.forced ? 'Session Ended (Forced)' : 'Session Ended';
  $('sessionGameTitle').textContent = title;
  $('sessionDuration').textContent = formatDuration(data.durationSecs || 0);
  $('sessionNotes').value = '';
  $('sessionSpoiler').checked = false;
  $('sessionHidePublic').checked = false;
  const hasNotes = mapping.type === 'live' || mapping.type === 'session';
  const notesWrap = $('sessionNotesWrap');
  if (notesWrap) notesWrap.hidden = !hasNotes;
  $('sessionError').textContent = '';
  showView('sessionView', { height: hasNotes ? 284 : 155 });
}

async function onSubmitSession(e) {
  e.preventDefault();
  if (!pendingSession) return;
  const mapping = pendingSession.mapping || {};
  const gameId = mapping.froglogId;
  const gameType = mapping.type || 'regular';
  const hasNotes = gameType === 'live' || gameType === 'session';
  const notes = hasNotes ? ($('sessionNotes').value.trim() || null) : null;
  const spoiler = hasNotes ? $('sessionSpoiler').checked : false;
  const isPublic = hasNotes ? !$('sessionHidePublic').checked : true;
  const errEl = $('sessionError');
  errEl.textContent = '';
  const rawHours = (pendingSession.durationSecs || 0) / 3600;
  const hours = roundHoursForFroglog(rawHours);
  try {
    const result = await invoke('submit_session', {
      gameType,
      gameId,
      hours,
      notes,
      spoiler,
      isPublic,
      title: (pendingSession.mapping && pendingSession.mapping.title) || null,
    });
    if (result && result.queued) {
      errEl.style.color = 'orange';
      errEl.textContent = 'Submission failed, session saved to Pending Submissions. Re-login or check your connection, then retry from the tray.';
      invoke('refresh_tray_menu').catch(() => {});
      return;
    }
    errEl.style.color = '';
    pendingSession = null;
    invoke('hide_window').catch(() => {});
  } catch (err) {
    errEl.textContent = String(err);
  }
}

function onSkipSession() {
  pendingSession = null;
  invoke('hide_window').catch(() => {});
}

// Session-ended event from backend
listen('session-ended', (event) => {
  showPostPlay(event.payload);
});

// Tray "Configure…" opens window and asks frontend to show mappings view
listen('open-mappings', () => {
  showView('mappingsView');
  loadMappingsView();
});

// Tray "About" opens main view
listen('show-main', () => {
  showView('mainView');
  loadMainView();
});

// Tray "Pending Submissions" opens pending view
listen('show-pending', () => {
  showView('pendingView');
  loadPendingView();
});


// Tray "Logout" (or after logout) show login view
listen('show-login', () => {
  showView('loginView');
});

// Init: check if we have auth
async function init() {
  loadVersion();
  try {
    const auth = await invoke('get_auth_config');
    if (auth && auth.token) {
      showView('mainView');
      loadMainView();
      return;
    }
  } catch (_) {}
  showView('loginView');
}

// Render app shell first (elements must exist before we bind or call init)
const app = document.getElementById('app');
app.innerHTML = `
  <div data-view id="loginView">
    <div class="page-header"><h2>LilyPad – Login</h2><span class="app-version"></span></div>
    <p class="muted">Enter your FrogLog credentials. The app runs in the system tray and tracks game time when you have process mappings set up.</p>
    <form id="loginForm">
      <label>Username</label>
      <input type="text" id="username" required autocomplete="username" />
      <label>Password</label>
      <input type="password" id="password" required autocomplete="current-password" />
      <label class="mappings-auto-submit-label"><input type="checkbox" id="rememberMe" checked /> Remember me</label>
      <p id="loginError" class="error"></p>
      <button type="submit">Log in</button>
    </form>
  </div>
  <div data-view id="mainView" hidden>
    <div class="page-header">
      <h2>LilyPad <small class="about-subtitle">for FrogLog</small></h2>
      <span class="app-version"></span>
    </div>
    <p id="pendingNotice" class="pending-notice" hidden></p>
    <p>A lightweight system tray companion for <a href="#" class="ext-link" data-url="https://froglog.co.uk/">FrogLog</a>, the personal game tracking app. LilyPad watches for game processes in the background and prompts you to log a session when you stop playing.</p>
    <hr />
    <p class="muted about-steps-heading"><strong>Getting started</strong></p>
    <ol class="about-steps muted">
      <li>Right-click the tray icon and choose <strong>Configure…</strong> to link each game's <code>.exe</code> to its FrogLog entry.</li>
      <li>Launch your game as normal — LilyPad will detect it automatically.</li>
      <li>When you close the game, this window will appear so you can submit the session.</li>
    </ol>
  </div>
  <div data-view id="mappingsView" hidden>
    <div class="mappings-scrollable">
      <div class="page-header"><h2>Configuration</h2><span class="app-version"></span></div>
      <p id="mappingsPendingNotice" class="pending-notice" hidden></p>
      <p class="muted">Type the executable name (e.g. <code>game.exe</code>) in the exe column. Once a session ends, LilyPad will prompt you to log the session.</p>
      <label class="mappings-auto-submit-label"><input type="checkbox" id="mappingsAutoSubmitRegular" /> Auto-submit regular game sessions</label>
      <label class="mappings-auto-submit-label"><input type="checkbox" id="mappingsAutoSubmitSession" /> Auto-submit session-tracked game sessions</label>
      <label class="mappings-auto-submit-label"><input type="checkbox" id="mappingsAutoSubmitLive" /> Auto-submit live service sessions</label>
      <label class="mappings-auto-submit-label"><input type="checkbox" id="shareNowPlaying" /> Enable online presence on FrogLog</label>
      <div class="mappings-switch">
        <button type="button" id="mappingsSwitchGames" class="active">Games</button>
        <button type="button" id="mappingsSwitchSession">Session tracked</button>
        <button type="button" id="mappingsSwitchLive">Live service</button>
      </div>
      <div class="mappings-search-wrap"><input type="search" id="mappingsSearch" class="mappings-search" placeholder="Search…" /></div>
      <p id="mappingsError" class="error"></p>
      <div id="mappingsTableWrap" class="mappings-table-wrap">
        <table class="mappings-table">
          <thead><tr><th>Game</th><th>exe</th><th class="mappings-th-window-title">Window title filter</th></tr></thead>
          <tbody id="mappingsTableBody"></tbody>
        </table>
      </div>
      <div id="mappingsPagination" class="mappings-pagination" hidden>
        <button type="button" id="mappingsPrev"><svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" style="vertical-align:-2px"><polyline points="15 18 9 12 15 6"/></svg> Prev</button>
        <span id="mappingsPageInfo"></span>
        <button type="button" id="mappingsNext">Next <svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" style="vertical-align:-2px"><polyline points="9 18 15 12 9 6"/></svg></button>
      </div>
    </div>
    <div class="mappings-footer">
      <button type="button" id="mappingsApply" disabled>Apply</button>
    </div>
  </div>
  <div data-view id="sessionView" hidden>
    <div class="page-header"><h2 id="sessionHeader">Session Ended</h2><span class="app-version"></span></div>
    <p><strong id="sessionGameTitle"></strong> – <span id="sessionDuration"></span></p>
    <form id="sessionForm">
      <div id="sessionNotesWrap" hidden>
        <label>Notes (optional)</label>
        <textarea id="sessionNotes" rows="2"></textarea>
        <div class="session-checkboxes">
          <label class="session-check-label"><input type="checkbox" id="sessionSpoiler" /> Contains spoilers</label>
          <label class="session-check-label"><input type="checkbox" id="sessionHidePublic" /> Hide from public</label>
        </div>
      </div>
      <p id="sessionError" class="error"></p>
      <div class="session-form-actions">
        <button type="submit">Submit to FrogLog</button>
        <button type="button" id="skipSession">Do not record session</button>
      </div>
    </form>
  </div>
  <div data-view id="pendingView" hidden>
    <div class="page-header"><h2>Pending Submissions</h2><span class="app-version"></span></div>
    <p class="muted">These sessions failed to submit. Logout and back in from the system tray to refresh your token, or check your connection, then use the retry button to resubmit them.</p>
    <div id="pendingList" class="pending-list"></div>
    <div class="mappings-footer">
      <button type="button" id="pendingBack">Back</button>
    </div>
  </div>
`;

document.addEventListener('click', (e) => {
  const link = e.target.closest('.ext-link');
  if (link) {
    e.preventDefault();
    invoke('open_url', { url: link.dataset.url }).catch(() => {});
  }
});
document.getElementById('loginForm').addEventListener('submit', onLogin);
document.getElementById('sessionForm').addEventListener('submit', onSubmitSession);
document.getElementById('skipSession').addEventListener('click', onSkipSession);
$('mappingsSwitchGames').addEventListener('click', () => setMappingsMode('regular'));
$('mappingsSwitchLive').addEventListener('click', () => setMappingsMode('live'));
$('mappingsSwitchSession').addEventListener('click', () => setMappingsMode('session'));
$('mappingsPrev').addEventListener('click', () => { mappingsPage--; renderMappingsTable(); });
$('mappingsNext').addEventListener('click', () => { mappingsPage++; renderMappingsTable(); });
$('mappingsSearch').addEventListener('input', (e) => { mappingsSearch = e.target.value; mappingsPage = 0; renderMappingsTable(); });
$('mappingsApply').addEventListener('click', doApply);
function saveAutoSubmit() {
  invoke('save_auto_submit', { regular: !!$('mappingsAutoSubmitRegular').checked, live: !!$('mappingsAutoSubmitLive').checked, session: !!$('mappingsAutoSubmitSession').checked }).catch(() => {});
}
function saveShareNowPlaying() {
  const share = !!$('shareNowPlaying').checked;
  console.log('[LilyPad] saveShareNowPlaying toggled to:', share);
  invoke('save_now_playing_share', { share }).catch((err) => {
    console.error('[LilyPad] save_now_playing_share error:', err);
  });
}
$('mappingsAutoSubmitRegular').addEventListener('change', saveAutoSubmit);
$('mappingsAutoSubmitLive').addEventListener('change', saveAutoSubmit);
$('mappingsAutoSubmitSession').addEventListener('change', saveAutoSubmit);
$('shareNowPlaying').addEventListener('change', saveShareNowPlaying);
$('pendingBack').addEventListener('click', () => { showView('mainView'); loadMainView(); });
init();
