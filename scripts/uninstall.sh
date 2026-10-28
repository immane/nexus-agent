#!/bin/sh
set -eu

cat <<'EOF'
█▄ █ █▀▀ ▀▄▀ █ █ █▀▀   ▄▀█ █▀▀ █▀▀ █▄ █ ▀█▀
█ ▀█ ██▄ █ █ █▄█ ▄▄█ ▄ █▀█ █▄█ ██▄ █ ▀█  █
EOF

destination=${1:-"${HOME:?HOME must be set}/.local/bin"}
if [ "$#" -gt 1 ]; then
    printf 'Usage: %s [install-directory]\n' "$0" >&2
    exit 2
fi

printf 'Remove the following commands from %s?\n' "$destination"
printf '  nexus-tui, nexus-server, nexus-headless, nexus-config\n'
printf 'Configuration and credentials will not be removed. [y/N] '
IFS= read -r answer || exit 1
case "$answer" in
    y|Y|yes|YES) ;;
    *) printf 'Cancelled.\n'; exit 0 ;;
esac

for binary in nexus-tui nexus-server nexus-headless nexus-config; do
    if [ -f "$destination/$binary" ] || [ -L "$destination/$binary" ]; then
        rm -- "$destination/$binary"
        printf 'Removed %s\n' "$destination/$binary"
    fi
done
printf 'Uninstall complete. Configuration and credentials were retained.\n'
