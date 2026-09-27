# WinDoze

Put the background apps **you choose** to sleep (**doze**) so the app you're using gets the RAM and CPU. Switch back and they **wake** up where you left them.

You pick the apps (e.g. Figma, Claude, Slack). For each one you choose **when** it dozes and **after how many minutes**:

| Mode | Dozes when… |
|---|---|
| **When minimized** | every window of the app has been minimized for N minutes |
| **When not in focus** | you've been using other apps for N minutes (even if its window is visible) |
| **When idle** | it's not in focus **and** using almost no CPU (default < 1% of total) **and** not playing audio, for N minutes |

`0 min` means "after 5 seconds", so a quick Alt-Tab never dozes anything.

Switching back to a dozing app wakes it: click its taskbar button or thumbnail, Alt-Tab to it, or click its window. Under the hood, dozing means all of the app's processes are suspended. Optionally, WinDoze also pushes a dozing app's memory out of RAM ("Free its memory"). Because the app's threads are suspended, it can't pull that memory straight back in, which is where ordinary "RAM cleaners" fail.

## Safety rails

- **Opt-in only.** Nothing dozes unless you add it. Windows and security processes (explorer, dwm, csrss, Defender, WSL/`vmmem`, …) can't be dozed even if you try.
- **Child processes are separated.** Terminals, shells and anything they start (for example `npm run dev` in Cursor's terminal) never doze with their parent app. The list is editable in Settings. A browser opened by clicking a link inside an app is also left alone. Caveat: see Known limits.
- **Dozing is skipped while an app:**
  - is playing audio
  - owns the clipboard (pasting from a dozing app can hang)
  - has no open window (tray-only apps can't be woken by switching to them)
- **All-or-nothing.** If any process of an app can't be suspended, none are.
- **Many ways to wake:**
  - switching to the app
  - the tray menu (**Wake all**)
  - the panic key **Ctrl+Alt+Shift+T** (wakes everything and pauses)
  - pausing WinDoze or quitting it
  - signing out or shutting down
- **Crash-safe.** Every dozing process is recorded in `%APPDATA%\WinDoze\dozing.json`. If WinDoze crashes, panics or is killed from Task Manager, a small watchdog process, or the next launch, wakes everything.
- **Log file:** `%APPDATA%\WinDoze\windoze.log` (Settings → Open log folder).

## Run it

Download `WinDoze.exe` from the [latest release](https://github.com/AkashKhamkar/WinDoze/releases/latest) and double-click it. No install and no admin rights are needed. It lives in the system tray, and closing the window keeps it running.

> Windows SmartScreen / Defender may warn about an unsigned app that suspends other processes. That's expected for an unsigned build.

## Build

On Windows:

```
cargo build --release          # -> target\release\windoze.exe
```

Cross-compiling from macOS/Linux:

```
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64         # or your distro's mingw-w64 package
cargo build --release --target x86_64-pc-windows-gnu
```

CI (`.github/workflows/build.yml`) builds the exe on `windows-latest` and uploads it as an artifact. Pushing a version tag (e.g. `git tag v0.1.0 && git push origin v0.1.0`) builds the exe and publishes it as a GitHub Release.

## First-run test checklist

Use a real Windows PC. A VM is fine for checking basics, but its memory behaviour won't match an 8 GB laptop, and some VMs lack OpenGL (you'll get an error box).

1. Open Figma (or any Electron app). In WinDoze, click **Add** next to it and choose **When minimized**, **0 min**.
2. Minimize Figma. Within ~5 s its status turns **Dozing** and shows the memory freed. In Task Manager → Details, its processes show **Suspended**.
3. Click Figma on the taskbar. It should wake and restore right away.
4. Let it doze again, then check the other ways back in:
   - hover over its taskbar button and click the thumbnail
   - Alt-Tab to it
   - set it to **When not in focus**, leave its window visible, let it doze, then click on the window
   - tray → **Wake all**, then **Ctrl+Alt+Shift+T**

   If any of these doesn't wake it, open the log (Settings → Open log folder) and send the lines starting with `taskbar click on` or `switcher`. They show what WinDoze saw.
5. Doze it, then kill WinDoze: in Task Manager → **Details**, right-click the `windoze.exe` with the **highest RAM** (the main one, not the ~1 MB watchdog) → **End task**. Figma should be woken by the watchdog within a second. The log shows `watchdog: WinDoze (pid …) exited`.
6. Try **When idle** with a YouTube video playing in a browser. It should stay "Active: Playing audio".

## Known limits

- **How waking works.** A dozing app can't answer when you click its taskbar button or pick it in Alt-Tab, so WinDoze works out which app you meant. It reads the name of the button under your mouse, or the item highlighted in Alt-Tab, using the Windows accessibility API (the same one screen readers use). It then wakes that app and brings its window back.
- **Unidentified taskbar clicks wake everything.** If you click a running app's taskbar button, WinDoze can't tell which app it is, and nothing opens within 0.7 s, it wakes all dozing apps. That's deliberate: it's better than leaving you stuck. This safety net recognises English taskbar buttons ("… running window"); name matching works in any language.
- **No waking from notifications or the tray.** A dozing app's notifications, tray icon and background sync don't work until it's woken. Apps with no open window never doze for this reason.
- **Editor terminals can stall.** When an editor like Cursor or VS Code is dozing, its terminal's shell and dev server keep running, but the editor's own process reads their output. A process that prints a lot (e.g. a verbose build or dev-server logs) will block once the output buffer fills, until you switch back to the editor. For long builds, use **When minimized** with a longer delay, or leave the editor out.
- **Freed memory isn't new memory.** It goes to Windows' compressed memory or the pagefile. Switching back to an app whose memory went to disk can take a second or two.

## Credits

App icon: 😴 "Sleeping Face" from [Noto Emoji](https://github.com/googlefonts/noto-emoji) by Google, licensed under the [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0). Source image and generated sizes are in `assets/`.
