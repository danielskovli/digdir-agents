#!/bin/sh
set -eu

# Clones each repository listed in /etc/agent/repositories into /home/agent/code, one per line:
# `owner/name` for GitHub, or any URL Git can clone. Blank lines and `#` comments are ignored. A
# checkout that already has `.git`, such as a bind-mounted host checkout, is left alone.
list=/etc/agent/repositories
code=/home/agent/code
[ -r "$list" ] || exit 0

missing=
while read -r repository _; do
    case "$repository" in "" | "#"*) continue ;; esac
    name=${repository##*/}
    name=${name%.git}
    [ -e "$code/$name/.git" ] || missing="$missing $repository"
done <"$list"
[ -n "$missing" ] || exit 0
mkdir -p "$code"

# Guest boot can race the host-mediated network handshake. Wait for DNS, while leaving each
# clone itself as one best-effort attempt.
remaining=30
while ! getent ahosts github.com >/dev/null 2>&1; do
    if [ "$remaining" -eq 0 ]; then
        echo "github.com did not become resolvable within 30 seconds" >&2
        exit 1
    fi
    remaining=$((remaining - 1))
    sleep 1
done

status=0
for repository in $missing; do
    case "$repository" in
    *://* | *@*) url=$repository ;;
    *) url="https://github.com/$repository.git" ;;
    esac
    name=${repository##*/}
    git clone "$url" "$code/${name%.git}" || status=1
done
exit "$status"
