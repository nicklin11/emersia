#!/usr/bin/env bash
# Repository hygiene checks — fast, dependency-free, runs in CI (job: validation).
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
err() { echo "::error::$*" >&2; fail=1; }

# LICENSE must be GPL-3.0
if ! head -n 5 LICENSE 2>/dev/null | grep -q "GNU GENERAL PUBLIC LICENSE"; then
  err "LICENSE is missing or not GPL-3.0"
fi
if ! grep -q "Version 3" LICENSE; then
  err "LICENSE is not version 3"
fi

# README must exist and introduce the project
if [ ! -s README.md ] || ! grep -q "^# Emersia" README.md; then
  err "README.md missing or lacks a top-level '# Emersia' heading"
fi

# ADR hygiene: docs/adr/NNNN-kebab.md, first line '# ADR NNNN: …', unique numbers
declare -A seen=()
shopt -s nullglob
for f in docs/adr/*.md; do
  base="$(basename "$f")"
  num="${base%%-*}"
  if ! [[ "$num" =~ ^[0-9]{4}$ ]]; then
    err "ADR file '$f' does not start with a 4-digit number"
    continue
  fi
  if [ -n "${seen[$num]:-}" ]; then
    err "duplicate ADR number $num ($f)"
  fi
  seen[$num]="$f"
  head1="$(head -n 1 "$f")"
  if [[ "$head1" != "# ADR $num:"* ]]; then
    err "ADR '$f' first line must be '# ADR $num: …' (got: $head1)"
  fi
done

# Workflows must exist
for wf in ci.yml project-automation.yml; do
  [ -f ".github/workflows/$wf" ] || err "missing workflow .github/workflows/$wf"
done

if [ "$fail" -ne 0 ]; then
  echo "pre-check FAILED" >&2
  exit 1
fi
echo "pre-check OK ($(ls docs/adr/*.md 2>/dev/null | wc -l) ADRs)"
