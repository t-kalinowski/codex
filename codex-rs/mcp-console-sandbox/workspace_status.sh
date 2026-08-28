#!/usr/bin/env bash
set -euo pipefail

revision="${STABLE_GIT_COMMIT:-}"
if [[ -z "${revision}" ]]; then
  revision="$(git rev-parse --verify HEAD)"
fi
if [[ ! "${revision}" =~ ^[0-9a-fA-F]{40}$ ]]; then
  printf 'invalid Git revision: expected a full 40-hex SHA\n' >&2
  exit 1
fi

workspace_version="$(
  awk -F'"' '
    $0 == "[workspace.package]" { in_workspace_package = 1; next }
    in_workspace_package && /^\[/ { exit }
    in_workspace_package && /^version = "/ { print $2; exit }
  ' codex-rs/Cargo.toml
)"
if [[ -z "${workspace_version}" || "${workspace_version}" =~ [[:space:]] ]]; then
  printf 'invalid Codex workspace version\n' >&2
  exit 1
fi

printf 'STABLE_GIT_COMMIT %s\n' "${revision}"
printf 'STABLE_CODEX_WORKSPACE_VERSION %s\n' "${workspace_version}"
