#!/usr/bin/env bash
#
# Build the release binary and install it as ~/.local/bin/sv.
#
#   ./install.sh

set -euo pipefail
cd "$(dirname "$0")"

bin_dir="$HOME/.local/bin"

cargo build --release

mkdir -p "$bin_dir"
# Copy then rename, so a running `sv` keeps its old file instead of having
# the binary rewritten underneath it.
cp target/release/sv "$bin_dir/sv.new"
chmod 755 "$bin_dir/sv.new"
mv -f "$bin_dir/sv.new" "$bin_dir/sv"

printf '\033[1;32minstalled\033[0m %s\n' "$bin_dir/sv"
case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) printf 'note: %s is not on your PATH\n' "$bin_dir" ;;
esac
