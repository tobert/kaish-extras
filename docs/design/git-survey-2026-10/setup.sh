#!/bin/sh
# Build the survey fixture: kaish-extras at the baseline commit, with one
# staged edit, one unstaged edit, one untracked file, a side branch, and a tag.
#   setup.sh <empty-dir> [source-repo]
set -e
D=${1:?usage: setup.sh <empty-dir> [source-repo]}
SRC=${2:-"$HOME/src/kaish-extras"}
git clone -q "$SRC" "$D"
cd "$D"
git checkout -q -B main 926edd4
git config user.email t@e.x; git config user.name T
git tag v0.1.0-survey 'HEAD~3'
git checkout -q -b feature/x
echo "feature line" >> docs/issues.md
git commit -qam "feature: extra commit"
git checkout -q main
echo "staged change" >> README.md
git add README.md
echo "unstaged change" >> Cargo.toml
echo hi > untracked.txt
