# ccmv

Moving a Claude Code project directory breaks every path Claude Code recorded
about it. `ccmv` performs the move and repairs those references in one run.
This file is the glossary for that domain — it defines terms, not behaviour.

## Projects and their data

**Project**:
A directory Claude Code has been used in, identified by its absolute path.
_Avoid_: Repo, workspace, folder

**Project directory**:
The directory on disk that a Project's path names. Holds the source tree and
the Local Claude Directory.
_Avoid_: Working directory, project root

**Local Claude Directory**:
The `.claude/` directory inside a Project directory. Travels with the Project
directory when it moves.
_Avoid_: Local config, dot-claude

**Global Session Directory**:
The directory under `~/.claude/projects/` holding a Project's sessions. Named
by the Encoded Path, so it does not travel with the Project directory — it has
to be renamed separately.
_Avoid_: Session dir, global dir, projects dir

**Encoded Path**:
A Project's absolute path with every character except alphanumerics and `-`
replaced by `-`. The name of its Global Session Directory.
_Avoid_: Slug, hash, key

**Subproject**:
A Project registered at a path nested inside another Project's path. Its
Project directory travels with the parent's move, but its Global Session
Directory is encoded from its own path and must be renamed on its own.
_Avoid_: Child project, nested repo

**Shared Files**:
`~/.claude.json` and `history.jsonl` — the two files that belong to no single
Project and are rewritten once per run.
_Avoid_: Global config

## Moving

**Move**:
One Project going from a source path to a target path.
_Avoid_: Migration (that is the whole run), transfer, relocation

**Run**:
One invocation of `ccmv`, carrying one or more Moves.
_Avoid_: Migration, batch (that is one way of specifying a Run)

**Unit**:
One Move together with everything the Run scanned about it before the first
write.
_Avoid_: Job, item, entry

**Substitution Set**:
The `old path -> new path` pairs of a Run, applied as one unit. Longest match
wins, and the result is never rescanned, so a Set containing both `a -> b` and
`b -> c` never carries an `a` through to `c`.
_Avoid_: Mapping, rewrite rules, replacements

**Segment Boundary**:
The condition that makes a path match end at a `/` or at the end of a path
component, so that moving `/x/proj` leaves `/x/proj_other` alone.
_Avoid_: Word boundary

**Session-only Move**:
A Move that relocates a Project's Claude Code data while leaving both Project
directories on disk untouched — for a directory the user already moved by hand.
_Avoid_: Metadata-only move, soft move

**Settled**:
A Unit whose source and target are the same path and whose references are
already consistent. Contributes no Substitution and is skipped by every step.
_Avoid_: No-op, done, clean

## Links

**Inbound Link**:
A symlink living outside a moving Project whose target resolves inside it.
These are what a Move breaks and what a Relink repairs.
_Avoid_: Backlink, incoming link, reverse dependency

**Self-referential Link**:
An Inbound Link whose own path is also inside the moving Project — both
endpoints move together.
_Avoid_: Internal link, local link

**Relink Candidate**:
A symlink the scan found whose resolved target matches the Substitution Set:
its path, its raw target, its resolved target, whether it was relative, and
whether it resolved before the Move.
_Avoid_: Match, hit, finding

**Alive** / **Dead**:
Whether a link's target existed *before* the Move. A Dead link is left alone —
after the Move the two are indistinguishable, and a link that has been broken
for months is not this Run's business.
_Avoid_: Valid/invalid, broken, dangling

**Relink**:
Repointing Relink Candidates at their new targets as part of a Move, when both
the old and the new path are known from the same invocation.
_Avoid_: Fix, update, repair (that is the other thing)

**Repair**:
Repointing links when the old path is no longer on disk and can only be
recovered from the broken links themselves. A separate entry point from
Relink, which always knows both paths.
_Avoid_: Relink, heal, restore
_Note_: the CEF command `/cef:repair-links` predates this term and does
something else — it consumes the Text Reference report. The name is taken;
one of the two has to give it up.

**Scan Root**:
A directory whose tree is searched for Relink Candidates. By default the
Local Claude Directory of every Project in `~/.claude.json`.
_Avoid_: Search path, source dir

**Text Reference**:
An absolute path to a moving Project appearing in a file's *contents* rather
than as a symlink target. Reported, never rewritten — changing one takes
judgement that belongs to the tool that owns the file.
_Avoid_: String reference, hardcoded path, occurrence

**Relink Log**:
The record of every link this Run repointed, one `path`/`old`/`new` row each.
The only account of the writes a Run made outside the moving Project, and so
the only way back from them.
_Avoid_: Journal, audit log, undo file
