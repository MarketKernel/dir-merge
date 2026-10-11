# DirMerge

A cross-platform (macOS, Windows, Linux) file and folder diff and merge tool in the spirit of WinMerge, written in Rust.

## Features

### File comparison

- Two panes with synchronized scrolling; lines are aligned and gaps are hatched.
- Syntax highlighting for 185 languages (Sublime Text grammars from `bat` via `syntect` + `two-face`), 32 color schemes.
- Changes within a line are highlighted word by word, refined down to characters. Similar lines inside a changed block are placed opposite each other.
- Merging: ⏵/⏴ buttons between the panes for every difference, copy all differences to one side, undo and redo.
- Editing: double-click a line to edit it; insert and delete lines from the context menu.
- Saving is atomic (via a temporary file). The original encoding, BOM and line endings are preserved.
- Search across both files, go to line, and an overview bar of all differences on the left.
- Comparison options: ignore whitespace (changes in amount, or all of it), case, blank lines.
- Encodings: UTF-8 (with or without BOM), UTF-16 LE/BE, single-byte and CJK encodings (Windows-1251, Shift_JIS, …) with auto-detection.
- Binary files are detected automatically.
- If a file is changed by another program, a banner offers to reload it.

### Folder comparison

- Recursive scanning in the background; files are compared in parallel (rayon).
- Comparison modes: by content, by size and date, by size only.
- A tree with per-folder totals; filters (identical, different, left only, right only) with counters; a name filter.
- Copy files and folders left or right, delete to the Trash.
- Exclusion masks (default `.git, .svn, .hg, .DS_Store, …`).
- Double-clicking a file opens its comparison in a new tab. After the file is saved, its row in the tree is refreshed.

### Large files

- Files of 4 MB and larger are opened via `mmap`: only the line index is kept in memory, and the OS pages the text in as it is read.
- Edits are stored in a line-based piece table. Merging a block does not copy text; it only references a range of lines in the other file.
- Rendering is virtualized: each frame draws only the visible lines. The scroll position is stored as `f64`, so scrolling stays precise even with tens of millions of lines.
- Syntax highlighting runs on a separate thread. The parser state is checkpointed every 256 lines. When jumping far ahead, parsing starts 400 lines before the target, so the end of a huge file is highlighted immediately.
- Example: two files of 1.5 million lines (67 MB each) load and compare in about 1.2 s on Apple Silicon.

## Building

Requires Rust 1.92 or newer.

Quick build with the script:

```sh
./build_scripts/build.sh                 # release build
./build_scripts/build.sh --debug         # debug build
./build_scripts/build.sh --test          # tests, then a release build
./build_scripts/build.sh --run a.rs b.rs # build and launch right away
```

Or manually:

```sh
cargo build --release
./target/release/dir-merge                  # start screen
./target/release/dir-merge left.rs right.rs # compare files
./target/release/dir-merge dirA dirB        # compare folders
```

On Linux, building requires the X11/Wayland headers:
`sudo apt install libxkbcommon-dev libwayland-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev`.

CI (`.github/workflows/ci.yml`) builds and tests the project on all three OSes.

## Keyboard shortcuts

| Action | Keys |
|---|---|
| Next / previous difference | Alt+↓ / Alt+↑ |
| Copy current difference right / left | Alt+→ / Alt+← |
| Save | Ctrl/⌘+S |
| Undo / redo | Ctrl/⌘+Z / Ctrl/⌘+Shift+Z |
| Find / next match | Ctrl/⌘+F, Enter, F3 |
| Go to line | Ctrl/⌘+L |
| Reload and compare again | F5 |
| New tab / close tab | Ctrl/⌘+T / Ctrl/⌘+W |

## Code layout

```
src/
  text/source.rs     mmap/buffer, line index, encoding and EOL detection
  text/document.rs   line-based piece table, saving
  diff/lines.rs      line diff (imara-diff, Histogram), display row model
  diff/align.rs      alignment of similar lines inside a changed block
  diff/inline.rs     word-level diff within a line
  highlight.rs       background syntax highlighting with checkpoints
  dircmp.rs          folder comparison engine
  ui/file_view.rs    file comparison view (custom rendering)
  ui/dir_view.rs     folder comparison view
  ui/home.rs         start screen
  app.rs             tabs, theme, settings, keyboard shortcuts
  autotest.rs        scripted UI driver for screenshot tests (`autotest` feature)
```

### Scripted UI checks

```sh
cargo build --release --features autotest
DIRMERGE_SCRIPT="wait 2; key alt+ArrowDown; shot out.png; quit" ./target/release/dir-merge a.txt b.txt
```

Script commands: `wait <sec>`, `key <mods+Key>`, `click x y`, `dclick x y`, `rclick x y`, `move x y`, `scroll <dy>`, `text <string>`, `shot <file.png>`, `quit`.
