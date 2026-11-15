#!/bin/sh
# view-by-hand.sh SOCKET [SECONDS]: what a voice service sends for the Agents view and the
# wave, by hand and as root (stop the voice service first: one voice client at a time).
# The view, its values, a strip text, then levels 20 times a second for SECONDS (default 5);
# the view stays 20 s more for taps. Prints every line the app sends: the answers and the
# view's events. Needs tb-surface beside it or on PATH.
S=${1:?usage: view-by-hand.sh SOCKET [SECONDS]}
N=$(( ${2:-5} * 20 ))
TOOL=$(dirname "$0")/../makepad-example-tinybot-surface/tb-surface
[ -x "$TOOL" ] || TOOL=tb-surface
(
echo '{"type":"view","name":"agents","title":"Agents","tree":[{"kind":"text","text":"Tap an agent to make it the default for voice."},{"kind":"each","of":"voice/agents","group":"host","item":{"kind":"pick","id":{"field":"id"},"text":{"field":"name"},"note":{"field":"note"},"dot":{"field":"dot"},"selected":{"is":"voice/default"},"set":"voice/default"}}]}'
echo '{"type":"put","state":{"voice/agents":[{"id":"opt","name":"tiny-monolith-optimizer","host":"linux-node","note":"idle","dot":"green"},{"id":"bld","name":"tinybot-building","host":"linux-node","note":"processing","dot":"amber"},{"id":"m2","name":"tinybot-m2","host":"mac-host","note":"ended","dot":"off"}],"voice/default":"opt"}}'
echo '{"type":"voice","text":"listening"}'
i=0
while [ "$i" -lt "$N" ]; do
	i=$((i + 1))
	echo "{\"type\":\"put\",\"state\":{\"voice/level\":0.$(( (i * 37 + i * i) % 90 + 10 ))}}"
	sleep 0.05
done
sleep 20
) | "$TOOL" --socket "$S" talk
