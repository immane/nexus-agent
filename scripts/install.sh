#!/bin/sh
set -eu

cat <<'EOF'
█▄ █ █▀▀ ▀▄▀ █ █ █▀▀   ▄▀█ █▀▀ █▀▀ █▄ █ ▀█▀
█ ▀█ ██▄ █ █ █▄█ ▄▄█ ▄ █▀█ █▄█ ██▄ █ ▀█  █
EOF

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
destination=${1:-"${HOME:?HOME must be set}/.local/bin"}
command -v cargo >/dev/null 2>&1 || { echo "cargo is required to build Nexus" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "python3 is required by nexus-config" >&2; exit 1; }
printf 'Building Nexus (release, container support enabled)...\n'
(cd "$repo_dir" && cargo build --release --locked --target-dir "$repo_dir/target" \
    -p nexus-tui -p nexus-server -p nexus-headless \
    --features nexus-tui/container,nexus-server/container --bins)
mkdir -p "$destination"
for binary in nexus-tui nexus-server nexus-headless; do
    install -m 755 "$repo_dir/target/release/$binary" "$destination/$binary"
done
install -m 755 "$script_dir/config.sh" "$destination/nexus-config"
printf 'Installed nexus-tui, nexus-server, nexus-headless and nexus-config to %s\n' "$destination"
printf 'Ensure %s is in your PATH.\n' "$destination"
printf 'Start the TUI with: nexus-tui\n'
