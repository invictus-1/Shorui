# Shorui

A fast PDF reader and editor for Windows. Rust, PDFium (the PDF engine in Chrome) and a GPU-drawn native UI. No webview, no account, no telemetry.

![Shorui with a document open](docs/screenshots/reader.png)

| Find and select | Highlight |
| --- | --- |
| ![Find in document](docs/screenshots/find.png) | ![Highlighted text](docs/screenshots/highlight.png) |

| Zoom to 6400% | Settings |
| --- | --- |
| ![Zoomed in](docs/screenshots/zoom.png) | ![Settings window](docs/screenshots/settings.png) |

## Download

Get `Shorui Setup.exe` from the [latest release](https://github.com/invictus-1/Shorui/releases/latest). It installs for the current user without administrator rights and takes under a second. Windows 10 or 11, 64-bit.

The installer is not code-signed, so Windows SmartScreen may warn the first time: choose "More info", then "Run anyway".

<img src="docs/screenshots/installer.png" alt="The Shorui installer" width="420">

## Build from source

You need Rust (stable, MSVC toolchain) on Windows.


```powershell
cargo run --release
```

Open a file directly:

```powershell
cargo run --release -- "C:\path\to\file.pdf"
```

The built app is `target\release\shorui.exe`. It needs `pdfium.dll` beside it; the build copies it there from `vendor\pdfium`.

## What it does

- Tabs, a thumbnail sidebar, continuous scrolling.
- Zoom from 10% to 6400% with Ctrl+wheel, pinch, or the toolbar. Zoom is anchored on the pointer.
- Fit width and fit page.
- Rotate pages, delete pages, save, save as. Select several pages in the sidebar with Ctrl+click or Shift+click.
- Find in document: matches stream in as pages are searched, ignoring case and line breaks.
- Select text by dragging, double-click a word, copy with Ctrl+C. Middle or right drag pans.
- Highlight selected text with H or the toolbar marker. Right-click a highlight to remove it. Highlights are saved into the PDF as standard annotations.
- Settings (the gear button, or Ctrl+,): zoom on open, thumbnails, highlight colour, updates and an about page.
- Password-protected PDFs.
- Drop files on the window to open them.

## Shortcuts

| Keys | Action |
| --- | --- |
| Ctrl+O | Open |
| Ctrl+S, Ctrl+Shift+S | Save, save as |
| Ctrl+W | Close tab |
| Ctrl+Tab, Ctrl+Shift+Tab | Next, previous tab |
| Ctrl+wheel, Ctrl++, Ctrl+- | Zoom |
| Ctrl+0, Ctrl+1, Ctrl+2 | 100%, fit width, fit page |
| Ctrl+R, Ctrl+Shift+R | Rotate right, left |
| Delete | Delete the pages selected in the sidebar |
| Ctrl+F | Find. Enter and Shift+Enter (or F3 and Shift+F3) step through matches, Esc closes |
| Ctrl+C, Ctrl+A | Copy the selected text, select all text |
| H | Highlight the selected text |
| Ctrl+B | Show or hide thumbnails |
| Ctrl+, | Settings |
| Page Up/Down, Space, arrows, Home, End | Move through the document |

## Updates and privacy

Shorui has no account and no telemetry. The only network request it ever makes is the update check: one call to GitHub's public API for this repository's latest release, sending nothing but a `Shorui/<version>` user agent. It runs when you press "Check for updates" and, unless you turn it off in Settings, at most once a day when the app starts. If a newer version exists, Settings offers to download its installer and run it.

Windows does not let an app make itself the default for a file type. "Make Shorui the default PDF app" in Settings opens Windows' own default-apps page for Shorui, where you confirm it.

## Installer

`installer/` builds `Shorui Setup.exe`, one file that installs, updates and removes Shorui. It needs no administrator rights.

```powershell
cargo build --release -p shorui; cargo build --release -p shorui-setup
```

The result is `target\release\shorui-setup.exe`. Build the app first: the installer packs `target\release\shorui.exe` and `pdfium.dll` into itself.

- Installs to `%LOCALAPPDATA%\Programs\Shorui` with a Start menu shortcut, an entry in Windows' installed apps list, and Shorui in "Open with" for PDFs. It does not change your default PDF app.
- Running it again updates in place and offers Uninstall. Uninstall is also in Windows' installed apps list.
- Uninstall removes only what the installer created: its three files, its shortcuts and its own registry keys.
- `--silent` installs with no window. `--uninstall --silent` removes with no window. `--dir <folder>` installs elsewhere, and `--no-register` skips shortcuts and the registry.

## How it stays fast

- `src/render.rs`: one worker thread owns PDFium and every open document. The UI thread never calls PDFium, so it never waits on a render.
- Pages are drawn as 1024px tiles, and only the tiles near the viewport are rendered. That keeps memory and render time flat at any zoom level.
- While you zoom, the tiles already on screen are scaled by the GPU. Sharp tiles replace them 80 ms after zoom stops, and a few times a second during a long zoom.
- Requests for tiles you have scrolled or zoomed past are cancelled before the worker gets to them.
- Search and text extraction run on the same worker one page at a time, behind tile rendering, so scrolling and zooming stay smooth while a search is running.
- PDFium loads and the file opens while the window is still being created.

`src/app.rs` is the UI and view state. `src/theme.rs` holds the design tokens from the Shorui design system.

## Measuring

Set `SHORUI_SELFTEST` to an output folder and pass a PDF. The app runs a scripted open, zoom, search, copy and edit, writes `log.txt` with timings and screenshots as `.bmp`, then exits. The edit step saves a copy as `edited.pdf` in that folder; the original is not changed.

## Icon

`assets/make_icon.py` draws the icon from the design system colours and writes `icon.ico` (16 to 256px), `icon-256.png` and `icon-64.rgba`. The build embeds the `.ico` in both the app and the installer. It needs Python with Pillow.

## PDFium

`vendor\pdfium\pdfium.dll` is build 8125 from <https://github.com/bblanchon/pdfium-binaries>. Its licence is in `vendor\pdfium\LICENSE`.

## Licence

MIT. See [LICENSE](LICENSE).
