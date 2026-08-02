# OBS Program

A Python automation tool that launches OBS Studio and runs several background processes to manage recordings, handle replay buffer clips, and display a screen overlay notification when a clip is saved.

---

## Features

- **Auto-starts OBS** — launches OBS minimized to tray with recording and replay buffer enabled
- **Clip hotkey** — press a configurable hotkey to save the current replay buffer as a clip, automatically organized into a folder named after the active window
- **Overlay notification** — briefly displays an image overlay on screen when a clip is saved
- **Auto-delete old recordings** — automatically removes the oldest recordings once a configurable limit is exceeded
- **Bug logging** — errors are logged to `bug_log.txt` with timestamps for easy debugging

---

## Requirements

Install the required Python packages:

```
pip install pygetwindow keyboard pillow
```

> **Note:** This program is designed for **Windows only** (uses Windows file paths and OBS's Windows executable).

---

## Configuration

All settings are stored in `configs.txt`. Edit this file before running the program:

```
overlay img name=overlay_img.png
overlay hotkey=ctrl+shift+`
obs path=C:\Program Files\obs-studio\bin\64bit\obs64.exe
recordings path=I:\OBS\Recordings
clips path=I:\OBS\Clips
number of recordings before deleting=48
```

| Setting | Description |
|---|---|
| `overlay img name` | Filename of the image to show as the clip overlay notification |
| `overlay hotkey` | Keyboard shortcut to trigger saving a replay clip |
| `obs path` | Full path to your `obs64.exe` executable |
| `recordings path` | Folder where OBS saves recordings and replay buffer files |
| `clips path` | Folder where saved clips will be moved and organized |
| `number of recordings before deleting` | Max number of recordings to keep; oldest are deleted beyond this limit |

---

## Usage

Run the program with:

```
python main.py
```

Or double-click the included `obs_start.py - Shortcut.lnk` shortcut.

### What happens when you run it:

1. **OBS launches** automatically, minimized to tray, and starts recording + replay buffer.
2. **Background threads** start managing recordings and listening for the hotkey.
3. **Press your hotkey** (default: `Ctrl+Shift+\``) at any time to save the current replay buffer as a clip:
   - An overlay image briefly appears in the top-right corner of your screen for 5 seconds as confirmation.
   - The clip is moved from the recordings folder into a subfolder inside the clips folder, named after the currently active window (e.g., `Clips\Minecraft\`).
4. **Old recordings are automatically deleted** in the background once the recording count exceeds the configured limit.

---

## Project Structure

```
OBS_program/
├── main.py              # Entry point — starts OBS and launches all threads
├── obs_start.py         # Launches OBS Studio via subprocess
├── msc_functions.py     # Core logic: clip handling, hotkey listener, auto-delete
├── overlay.py           # Tkinter overlay window shown when a clip is saved
├── config.py            # Reads and parses configs.txt
├── configs.txt          # User configuration file
├── bug_log.py           # Logs errors/exceptions to bug_log.txt
├── bug_log.txt          # Auto-generated error log
└── overlay_img.png      # Image displayed as the clip overlay notification
```

---

## How Clips Are Organized

When you press the hotkey, the program detects the **currently active window** and moves the replay file into a matching subfolder:

```
Clips/
├── Minecraft/
│   └── Replay_2024-01-01_12-00-00.mkv
├── League_of_Legends/
│   └── Replay_2024-01-02_15-30-00.mkv
```

Invalid filename characters are stripped and spaces are replaced with underscores.
