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

printf 'STABLE_GIT_COMMIT %s\n' "${revision}"
