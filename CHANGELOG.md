# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While pre-1.0, the minor version is bumped for breaking changes.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Fixed

- **Opening links on Windows** — a link's URL is no longer handed to
  `cmd /C start`, which read `&`, `|` and `^` in it as shell syntax:
  `https://example.com/?a=1&b=2` was cut off at the `&` and the rest ran as a
  command of its own, so a crafted link in the paged text could run arbitrary
  commands when opened. URLs now reach the default handler unaltered.
- **Opening links on more platforms** — links are opened through the
  [`open`](https://crates.io/crates/open) crate instead of a hard-coded launcher
  per OS, so they now also open on the BSDs, illumos and other Unix-likes, where
  nothing happened before; under WSL in the Windows default browser; and on Linux
  systems without `xdg-open`, falling back to `gio`, `gnome-open` or `kde-open`.

## [0.2.2] - 2026-04-16

### Fixed

- **Clipping Sixel and Kitty images** — images that are partly scrolled out of
  the viewport are now cut off cleanly at the top and bottom edge. Kitty images
  are decoded, cropped and re-encoded the way Sixel images already were, so only
  the visible rows are sent to the terminal.

## [0.2.1] - 2026-04-10

### Fixed

- **Status line in `-X` mode** — the status line is cleared on exit instead of
  being left behind on screen.

## [0.2.0] - 2026-04-09

### Added

- **`less`-compatible flags** — `-F` (quit if the content fits on one screen),
  `-R` (accepted for compatibility) and `-X` (don't use the alternate screen),
  with defaults read from the `LESS` environment variable, so lessi works as a
  drop-in Git pager.

### Fixed

- **Sixel detection with piped input** — the terminal is queried through
  `/dev/tty`, so Sixel support is detected even when the content comes in on
  stdin.

## [0.1.0] - 2026-04-08

### Added

- **Pager** — `less`-style navigation by line, half page, page and to start or
  end; `/` search with `n`/`N`; mouse wheel scrolling, drag to scroll and a
  draggable scrollbar.
- **ANSI rendering** — colors and text attributes are parsed and rendered
  faithfully.
- **OSC 8 hyperlinks** — Tab/Shift-Tab cycles through links, Enter or a click
  opens one, and links highlight on hover.
- **Inline images** — Sixel and Kitty graphics in the input are extracted and
  drawn at their position as you scroll, including multi-chunk Kitty transfers.

<!-- next-url -->
[Unreleased]: https://github.com/roblillack/lessi/compare/v0.2.2...HEAD
[0.2.2]: https://github.com/roblillack/lessi/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/roblillack/lessi/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/roblillack/lessi/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/roblillack/lessi/compare/d933cc4ae666bd7947044c79798a702ba589f008...v0.1.0
