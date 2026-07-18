# ccmv — Move and rename Claude Code projects

[![Crates.io](https://img.shields.io/crates/v/ccmv.svg)](https://crates.io/crates/ccmv)
[![CI](https://github.com/seflue/ccmv/actions/workflows/ci.yml/badge.svg)](https://github.com/seflue/ccmv/actions/workflows/ci.yml)

Move or rename [Claude Code](https://docs.anthropic.com/en/docs/claude-code) projects without losing conversation history or settings.

## The problem

Claude Code stores project context at `~/.claude/projects/` using a path derived from the project directory. Session files, settings, and history entries all contain the absolute path. Moving or renaming the project directory breaks these references. `claude -c` says "no conversations to continue".

## What ccmv does

Updates all path references so Claude Code keeps working after a move or rename:

- Session files (`.jsonl`) and subagent sessions
- Sessions index (`fullPath`, `projectPath`)
- Project settings (`permissions.allow`, hooks, `additionalDirectories`)
- MCP config (`.mcp.json`)
- History index (`~/.claude/history.jsonl`)
- Global project directory (`~/.claude/projects/{encoded}/`)
- Project trust and tool permissions (`~/.claude.json`)
- Sub-projects nested inside the moved directory

A backup is created automatically before each operation — one archive per project, so a batch produces one per move.

## Install

```
cargo install --path .
```

## Usage

```bash
ccmv myapp ~/work/
# moves to ~/work/myapp
```

Works like `mv`. If the target is an existing directory, the project is moved into it. An explicit target path works too:

```bash
ccmv ~/projects/myapp ~/work/myapp
```

Renaming is just a move to a sibling path:

```bash
ccmv ~/projects/old-name ~/projects/new-name
```

### Moving many projects

Several sources and a target directory, again like `mv`:

```bash
ccmv ~/projects/api ~/projects/web ~/projects/cli ~/work/
```

For longer lists, read the moves from a tab-separated file — one `SOURCE<TAB>TARGET` per line, `#` comments and blank lines skipped:

```bash
ccmv --batch moves.tsv
```

```
# moves.tsv
/home/user/projects/api	/home/user/work/backend/api
/home/user/projects/web	/home/user/work/frontend/web
```

`-` reads the plan from standard input, which is where globbing pays off:

```bash
for p in ~/projects/*/; do
  printf '%s\t%s\n' "${p%/}" "$HOME/work/$(basename "$p")"
done | ccmv --batch -
```

Unlike positional arguments, a batch line states its target outright — the source name is not appended.

The whole plan is checked before the first write. Rejected are duplicate sources, two sources mapping to one target, chains (`a → b` together with `b → c`), and any two moves whose paths nest one inside the other. All problems are reported at once, with line numbers, so a 120-line plan does not need 120 runs to fix.

Two limitations worth knowing:

- **Paths containing tabs are not supported.** A line with a stray tab aborts the batch rather than guessing which two fields were meant.
- **A batch is not a transaction.** Validation and the pre-flight check catch what they can before anything is written, but a failure partway through leaves the earlier moves applied. The backups are what gets you back.

### Preview changes

```bash
ccmv -n myapp ~/work/
```

Shows which files would be updated and how many path replacements each contains. Nothing is modified.

For a batch, `-n` prints the plan: one line per move plus the global directory it takes with it. Both the move list and the file list stop after ten entries; `-v` prints them whole.

### Backup and restore

```bash
ccmv backup ~/projects/myapp
ccmv restore ~/.claude/backups/ccmv/...-20260318-213553.tar.gz
```

Backups contain session files, settings, and MCP config as a `.tar.gz` archive. The project directory itself is not included (use git for that).

### Session-only mode

```bash
ccmv --session-only ~/myapp/.claude/worktrees/feature-x ~/myapp
```

Moves only the session data — global `projects/{encoded}/`, jsonl `cwd` fields, sessions index, `history.jsonl`, and the `~/.claude.json` trust key. Neither project directory is touched.

Use case: point a session started in a git worktree back at the parent repo, then drop the worktree. The source path does not need to still exist on disk; only its session data under `~/.claude/` is required.

### Flags

| Flag | Short | Description |
|------|-------|-------------|
| `--batch <FILE>` | | Read `SOURCE<TAB>TARGET` lines from a file; `-` reads stdin |
| `--dry-run` | `-n` | Preview changes, don't modify anything |
| `--verbose` | `-v` | Print every move and every updated file, not just the first ten |
| `--force` | | Overwrite if target already has Claude Code data |
| `--no-backup` | | Skip automatic backup |
| `--session-only` | | Migrate only session data, leave project directories untouched |

Flags go before positional arguments: `ccmv -n source target`

## How it works

ccmv scans the current state, computes the target state, and applies only what needs to change. This makes operations idempotent. If a migration was interrupted or the directory was already moved manually, running ccmv again picks up where it left off.

### Path encoding

Claude Code encodes project paths by replacing all non-alphanumeric characters (except `-`) with `-`:

```
/home/user/my_project    ->  -home-user-my-project
/home/user/.config/app   ->  -home-user--config-app
```

### What gets backed up

- `~/.claude/projects/{encoded}/` (sessions, subagents, index)
- `.claude/` inside the project (settings, hooks)
- `.mcp.json`
- `~/.claude.json` (project trust and permissions)

Stored at `~/.claude/backups/ccmv/{encoded}-{timestamp}.tar.gz`.

## Development

```bash
cargo test       # unit + E2E tests
cargo clippy     # pedantic lints enabled
```

E2E tests use isolated temp directories and never touch your real `~/.claude/`.

## License

MIT
