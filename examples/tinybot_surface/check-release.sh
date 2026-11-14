#!/bin/sh
# check-release.sh BINARY...: fails when a binary was built with the test feature
# `test-userns-root`, under which the app takes a root of a user namespace for root. Such a
# build says so on stderr when it starts; that sentence must not be in a binary that ships.
# Prints one line per binary (the count of the sentence in it) and exits 1 on any hit.
marker='TEST BUILD: a peer that is root of a user namespace counts as root'
bad=0
for binary in "$@"; do
	[ -f "$binary" ] || { echo "check-release: $binary: no such file"; bad=1; continue; }
	hits=$(grep -c -a -F "$marker" "$binary")
	echo "check-release: $binary: test-userns-root marker count $hits"
	[ "$hits" = 0 ] || bad=1
done
[ $# -gt 0 ] || { echo "usage: check-release.sh BINARY..."; exit 64; }
exit $bad
