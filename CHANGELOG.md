# Changelog

All notable changes to this project are documented here.

## [Unreleased]

### Added

- `--relink`: repoint symlinks elsewhere on disk that point into the moved project. `--scan-root <DIR>` adds directories to search, on top of every registered project's `.claude/` tree.
- Text references — absolute paths to the moved project inside a file's contents — are reported in a `.textrefs.tsv`, never rewritten.
- Relink log (`.relink.tsv`), next to the backup archive, one row per repointed link, for manual rollback.

## [0.3.0] - 2026-07-19

### Added

- Batch mode: several sources into a target directory, or a plan of `SOURCE<TAB>TARGET` lines via `--batch <FILE>` (`-` reads stdin).
- Plans are checked as a whole before the first write, with every problem reported at once and named by line.
- Renames are checked up front: missing target directory, move across filesystems, unwritable target.

### Changed

- `-v` works; move and file lists are otherwise cut off after ten.
- `--session-only` merges into a target that already has session data instead of refusing it.
- Sub-project migration covers local settings and MCP config.

### Fixed

- Moving a project no longer rewrites paths of sibling projects whose name starts the same way. This could drop a foreign project's trust state and tool permissions from the global config.
- The consistency check matches paths at a segment boundary.

## [0.2.0] - 2026-05-17

### Added

- `--session-only`: move the session data, leave both project directories where they are.
- Justfile and release pipeline.

## [0.1.0]

- Move or rename a Claude Code project along with its session data and every reference to it. `--dry-run`, `--force`, `--no-backup`, plus `backup` and `restore`.
