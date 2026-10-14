#!/bin/sh
# `make e2e`: builds the server image around the Linux kablo binary given as
# the first argument, starts it, waits for sshd to take a key login and runs
# the end-to-end scenarios (crates/kablo/tests/e2e.rs) against it. The
# container, the key and the build context are removed on the way out, whatever
# the way.
set -eu

binary=${1:?usage: run.sh PATH-TO-THE-LINUX-KABLO-BINARY}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
work=$(mktemp -d)
container=

cleanup() {
	[ -n "$container" ] && docker rm -f "$container" >/dev/null 2>&1
	rm -rf "$work"
	return 0
}
trap cleanup EXIT INT TERM

cp "$here/Dockerfile" "$here/bashrc" "$work/"
cp "$binary" "$work/kablo"
ssh-keygen -q -t ed25519 -N '' -C kablo-e2e -f "$work/key"
cp "$work/key.pub" "$work/authorized_keys"

# Not shared with any other project's image: this one is kablo's own.
docker build -q -t kablo-e2e -f "$work/Dockerfile" "$work" >/dev/null
container=$(docker run -d --rm -p 127.0.0.1::22 kablo-e2e)
port=$(docker port "$container" 22/tcp | sed -n 's/.*://p' | head -n 1)

ssh_options="-p $port -i $work/key -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=5"
tries=0
until ssh $ssh_options tester@127.0.0.1 kablo version >/dev/null 2>&1; do
	tries=$((tries + 1))
	if [ "$tries" -ge 40 ]; then
		echo "e2e: the server never took a login" >&2
		docker logs "$container" >&2 || true
		exit 1
	fi
	sleep 0.5
done

cd "$root"
KABLO_E2E_PORT=$port KABLO_E2E_KEY=$work/key KABLO_E2E_CONTAINER=$container \
	cargo test -p kablo --test e2e -- --ignored --test-threads=1
