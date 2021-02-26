#!/bin/bash

SCRIPTS_ROOT=$(dirname $(realpath $0))
PROJECT_ROOT=$(dirname $SCRIPTS_ROOT)

cd $PROJECT_ROOT

cat $SCRIPTS_ROOT/NOTICE_template.md <( \
    cargo tree --prefix none -f '* [{p}]({r}) - {l}' \
    | perl -pe "s/ \\(\\*\\)$//" \
    | perl -pe "s/ - $/ - \\(\\*\\*Nonstandard License\\*\\*, see project link\\)/" \
    | sort | uniq \
) > $PROJECT_ROOT/NOTICE.md
