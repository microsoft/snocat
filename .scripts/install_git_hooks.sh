#!/usr/bin/env bash
cd `git rev-parse --show-toplevel`

echo "Installing hooks:"

HOOKS_TO_INSTALL=(
  pre-commit
)
for hookname in "${HOOKS_TO_INSTALL[@]}"; do
  printf "%s" "$hookname"
  ln -ns "../../.scripts/git_hooks/$hookname" ".git/hooks/$hookname" 2>/dev/null && \
    ( echo " - installed" || true ) || \
    echo " - already installed"
done
