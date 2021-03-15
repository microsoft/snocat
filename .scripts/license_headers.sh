#!/bin/bash

set -euo pipefail

SCRIPTS_ROOT=$(dirname $(realpath $0))
PROJECT_ROOT=$(dirname $SCRIPTS_ROOT)

TMPFILE=`mktemp`
cat <<- 'EOF' > $TMPFILE
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
EOF

function update_licenses () (
  TARGET_DIR="$1"
  TMPFILE="$2"
  shopt -s globstar
  echo "Searching for $TARGET_DIR/**/*.rs"
  for i in $(git ls-files -cmo "$TARGET_DIR/**/*.rs" || true)
  do
    printf "Testing %s... " "$i"
    if ! grep -q Copyright $i
    then
      echo "Updating..."
      cat $TMPFILE $i > $i.new && mv $i.new $i
    else
      echo "Passed."
    fi
  done
)

update_licenses "$PROJECT_ROOT/snocat/src" "$TMPFILE"
update_licenses "$PROJECT_ROOT/snocat-cli/src" "$TMPFILE"
