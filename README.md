# Lilypad

System tray app for the Froglog suite: tracks game play time and submits sessions or updates hours to your Froglog backend.

## Requirements

- Node.js and npm (for Tauri CLI)
- Rust (stable)
- Windows or Linux (process detection and tray supported on both)

## Development

```bash
cd lilypad
npm install
npm run dev
```

Build:

```bash
npm run build
```

## App icon

Put your own icons in **`lilypad/src-tauri/icons/`**. The config expects:

| File | Use |
|------|-----|
| `icon.ico` | Windows app + taskbar/tray |
| `32x32.png` | Small icon |
| `128x128.png` | Medium |
| `128x128@2x.png` | Retina |
| `icon.icns` | macOS (if you build on Mac) |

**From a single image:** use Tauri’s icon generator (one PNG, at least 512×512 recommended):

```bash
cd lilypad
npm run tauri icon path/to/your-icon.png
```

That writes all required sizes into `src-tauri/icons/`. If you only add **`icon.ico`** (e.g. for Windows), the app will use it for the window and system tray; the build currently creates a default green `icon.ico` when the file is missing.

## Configuration

- **API URL**: Set at first login (default: `https://api.froglog.co.uk/api`). Stored with token in:
  - Windows: `%APPDATA%/froglog-lilypad/auth.json`
  - Linux: `~/.config/froglog-lilypad/auth.json`
- **Process → Game mapping**: Stored in the same directory as `process-map.json`. Add entries like:
  ```json
  {
    "mappings": [
      { "process": "hl2.exe", "type": "regular", "froglogId": 42, "title": "Half-Life 2" }
    ]
  }
  ```
  On Linux, native executables have no `.exe` suffix (e.g. `"process": "hl2"`). Wine/Proton games still use `.exe` names as they appear in the process list.

  When an unknown process triggers a session, you can submit and then add a mapping (future: "Remember this process" in the UI).

## How to use

1. **Start the app** – Run `npm run dev` (or the built exe). The app minimizes to the **system tray**; the main window may stay hidden.
2. **Open the window** – Right‑click the Lilypad icon in the tray → click **Show** or **Settings**. The main window opens.
3. **First time** – You’ll see the **Login** form. Enter API URL (default `https://api.froglog.co.uk/api`), your Froglog username and password, then **Log in**. Next time you open the window you’ll see the main view.
4. **Add process mappings** – Right-click the tray icon → **Configure…**, or edit the config file directly:
   - Windows: `%APPDATA%\froglog-lilypad\process-map.json`
   - Linux: `~/.config/froglog-lilypad/process-map.json`
   ```json
   { "mappings": [
     { "process": "hl2.exe", "type": "regular", "froglogId": 42, "title": "Half-Life 2" },
     { "process": "WarThunder.exe", "type": "live", "froglogId": 5, "title": "War Thunder" }
   ]}
   ```
   On Linux, use the executable name without `.exe` for native games (e.g. `"process": "hl2"`). Use your real Froglog game IDs from the website (backlog = regular, live service = live).
5. **When you play** – Start a game whose executable is in the mapping. Lilypad detects it and starts timing. When you close the game, the main window opens with **Session ended**: time played, optional note, and **Submit to Froglog** (or **Skip**).

## How it works

1. Runs in the system tray.
2. Polls running processes every 10s and matches them against your configured mappings.
3. When a mapped game process starts, a session timer starts.
4. When the process exits, the window opens and shows time played; you can add a note and submit as a new live-service session or update total hours for a regular game.
