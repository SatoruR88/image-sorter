# Image Sorter

Lightweight Windows desktop app for manually sorting large numbers of images
into subfolders. Rust + Tauri + vanilla TypeScript. No frameworks, no database —
the filesystem is the source of truth.

## Run

```
npm install
npm run tauri dev      # development
npm run tauri build    # release build (.msi / .exe in src-tauri/target/release)
```

## Workflow

Open a folder. Images **directly inside** it form the queue; its **direct child
folders** become classification destinations.

## Shortcuts

| Key        | Action                                   |
| ---------- | ---------------------------------------- |
| `1`–`9`    | Move current image into child folder N   |
| `→` / `←`  | Next / previous image (no file changes)  |
| `Delete`   | Send current image to the Recycle Bin    |
| `Ctrl+Z`   | Undo the most recent move                |
| `+` button | Create a new category folder             |

## Behavior notes

- Filename collisions auto-rename: `IMG_001.jpg` → `IMG_001_1.jpg`, `_2`, …
- Deleting uses the Windows Recycle Bin; nothing is permanently deleted.
- `Ctrl+Z` restores moved files. It does **not** restore files sent to the
  Recycle Bin — recover those from the Recycle Bin itself.
- The watched folder updates live: creating/removing child folders in Explorer
  updates the destination buttons; externally added/removed images update the
  queue.
- The last used folder and window position/size are restored on restart
  (`%APPDATA%/com.rikun.image-sorter`).

## Layout

```
src-tauri/src/
  fs_ops.rs    scanning, move + collision handling, mkdir, recycle bin
  state.rs     session: queue, index, undo history, watcher handle
  settings.rs  last-folder persistence
  lib.rs       Tauri commands, filesystem watcher, app setup
src/
  main.ts      UI rendering, keyboard controls, bounded preloading
  styles.css   minimal dark theme
```
