#!/usr/bin/env bash
#
# Remove what ./install.sh put in ~/.local/bin. Leaves this repository, your
# config (~/.config/sakharov) and the editor's state directory alone.
#
#   ./uninstall.sh

set -euo pipefail

bin_dir="$HOME/.local/bin"
repo="$(cd "$(dirname "$0")" && pwd)"

removed=0
# `sv.new` is the temporary copy an interrupted install can leave behind.
for file in "$bin_dir/sv" "$bin_dir/sv.new"; do
    if [ -e "$file" ]; then
        rm -f "$file"
        printf 'removed %s\n' "$file"
        removed=1
    fi
done

if [ "$removed" = 1 ]; then
    printf '\033[1;32muninstalled\033[0m the sv binary\n'
else
    printf 'nothing to uninstall: no sv binary in %s\n' "$bin_dir"
fi
printf 'The repository at %s was not removed, and neither was your config in ~/.config/sakharov.\n' "$repo"
