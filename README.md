# ccmv — Move and rename Claude Code projects

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

A backup is created automatically before each operation.

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

### Preview changes

```bash
ccmv -n myapp ~/work/
```

Shows which files would be updated and how many path replacements each contains. Nothing is modified.

### Backup and restore

```bash
ccmv backup ~/projects/myapp
ccmv restore ~/.claude/backups/ccmv/...-20260318-213553.tar.gz
```

Backups contain session files, settings, and MCP config as a `.tar.gz` archive. The project directory itself is not included (use git for that).

### Flags

| Flag | Short | Description |
|------|-------|-------------|
| `--dry-run` | `-n` | Preview changes, don't modify anything |
| `--verbose` | `-v` | Detailed output |
| `--force` | | Overwrite if target already has Claude Code data |
| `--no-backup` | | Skip automatic backup |

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
