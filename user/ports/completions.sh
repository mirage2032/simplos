#!/usr/bin/env bash
# compinit's record of what the completion functions complete, for the tree
# of simplos's disk at TREE, in its /root/.zcompdump: which compinit checks
# against the functions there are, by their count, as root's zsh starts, and
# uses if it's right, in a tenth of a second, where making it again there
# takes eight. So it's made once every tree is in, as any of them may bring
# functions of its own (Rust's has _cargo), by the userland's own zsh, with
# fpath as zsh has it on simplos. Nothing to do without that zsh.
#
# Usage: user/ports/completions.sh TREE
set -euo pipefail

tree=${1:?usage: user/ports/completions.sh TREE}
[[ -x $tree/bin/zsh ]] || exit 0
mkdir -p "$tree/root"
# Afresh: one the userland had would be kept if only its count were right.
rm -f "$tree/root/.zcompdump"
# Its fpath as it's built in, here rather than the host's.
"$tree/bin/zsh" -f -c 'fpath=("$1"/usr/{local/,}share/zsh/site-functions
    "$1"/usr/share/zsh/*/functions(N))
    autoload -Uz compinit && compinit -u -d "$1/root/.zcompdump"' zsh "$tree"
