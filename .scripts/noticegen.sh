#!/bin/bash

set -e

SCRIPTS_ROOT=$(dirname $(realpath $0))
PROJECT_ROOT=$(dirname $SCRIPTS_ROOT)

function gen_notice () {
    pushd $1
    cat $SCRIPTS_ROOT/NOTICE_template.md <( \
        cargo tree --color=never --prefix none -f '* [{p}]({r}) - {l}' \
        | perl -pe "s/\\[([^ ]+ v[0-9]+\\.[0-9]+\\.[0-9]+) \\([^\\)]*\\)\\]/[\$1]/g" \
        | perl -pe "s/ \\(\\*\\)$//" \
        | perl -pe "s/ - $/ - \\(\\*\\*Nonstandard License\\*\\*, see project link\\)/" \
	| grep -v "^\\* \\[snocat " \
	| grep -v "^\\* \\[snocat-cli " \
        | sort | uniq \
    ) > $1/NOTICE.md
    popd
}

gen_notice $PROJECT_ROOT
gen_notice $PROJECT_ROOT/snocat
gen_notice $PROJECT_ROOT/snocat-cli
