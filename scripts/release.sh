#!/usr/bin/env bash
# Release: pre-flight, bump, wait for CI green, tag.
# Requires: cargo-edit (cargo install cargo-edit), gh CLI logged in.
# Usage: scripts/release.sh <version>

set -euo pipefail
shopt -s inherit_errexit   # set -e applies inside $(...) too

# --- output helpers ---------------------------------------------------------

if [[ -t 1 ]] && [[ -z "${NO_COLOR:-}" ]]; then
  C_RESET=$'\e[0m'
  C_DIM=$'\e[2m'
  C_INFO=$'\e[36m'
  C_OK=$'\e[32m'
  C_FAIL=$'\e[31m'
else
  C_RESET="" C_DIM="" C_INFO="" C_OK="" C_FAIL=""
fi

info() { echo "${C_INFO}→${C_RESET} $*"; }
note() { echo "${C_DIM}  $*${C_RESET}"; }
ok()   { echo "${C_OK}✓${C_RESET} $*"; }
fail() { echo "${C_FAIL}✗${C_RESET} $*" >&2; exit 1; }

# --- low-level helpers ------------------------------------------------------

ci_run_id_for_head() {
  gh run list \
    --workflow ci.yml \
    --branch main \
    --commit "$(git rev-parse HEAD)" \
    --limit 1 \
    --json databaseId \
    --jq '.[0].databaseId'
}

current_cargo_version() {
  grep '^version = ' Cargo.toml | head -1 | cut -d'"' -f2
}

# --- steps ------------------------------------------------------------------

parse_args() {
  [[ $# -eq 1 ]] || fail "usage: $0 <version>"
  version=$1
  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
    || fail "version must be semver (e.g. 0.1.1)"
  readonly version
}

check_tools() {
  command -v cargo-set-version >/dev/null || fail "install cargo-edit: cargo install cargo-edit"
  command -v gh                >/dev/null || fail "install gh CLI"
}

pre_flight() {
  info "pre-flight"
  [[ -z "$(git status --porcelain)" ]]                            || fail "working tree not clean"
  [[ "$(git branch --show-current)" == "main" ]]                  || fail "not on main"
  git fetch origin
  [[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main)" ]] || fail "local main differs from origin/main"
  ! git rev-parse "v$version" >/dev/null 2>&1                     || fail "tag v$version already exists"
}

cargo_dry_run() {
  info "cargo publish --dry-run"
  cargo publish --dry-run
}

bump_if_needed() {
  local current higher
  current=$(current_cargo_version)
  [[ -n "$current" ]] || fail "could not read version from Cargo.toml"

  if [[ "$current" == "$version" ]]; then
    info "Cargo.toml already at $version, skipping bump"
    return
  fi

  higher=$(printf '%s\n%s\n' "$current" "$version" | sort -V | tail -1)
  [[ "$higher" == "$version" ]] || fail "would downgrade Cargo.toml from $current to $version"

  info "bumping $current → $version"
  cargo set-version "$version"
  cargo check --quiet
  git add Cargo.toml Cargo.lock
  git commit -m "release: v$version"
  git push origin main
}

wait_for_ci() {
  local run_id
  info "waiting for CI on $(git rev-parse --short HEAD)"
  sleep 5
  run_id=$(ci_run_id_for_head)
  [[ -n "$run_id" ]] || fail "no ci.yml run found for current commit"
  gh run watch --exit-status "$run_id"
}

create_tag() {
  info "tagging v$version"
  git tag "v$version"
  git push origin "v$version"
}

done_msg() {
  ok "tag pushed — release workflow running"
  note "watch: gh run watch"
}

# --- main -------------------------------------------------------------------

main() {
  parse_args "$@"
  check_tools
  pre_flight
  cargo_dry_run
  bump_if_needed
  wait_for_ci
  create_tag
  done_msg
}

main "$@"
