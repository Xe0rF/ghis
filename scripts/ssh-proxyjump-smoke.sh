#!/bin/bash
set -euo pipefail

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/ghis-ssh-proxyjump.XXXXXX")
network="ghis-ssh-$RANDOM-$$"
jump="ghis-jump-$RANDOM-$$"
target="ghis-target-$RANDOM-$$"
agent_pid=

cleanup() {
    status=$?
    if [ -n "$agent_pid" ]; then
        SSH_AGENT_PID=$agent_pid ssh-agent -k >/dev/null 2>&1 || true
    fi
    docker rm -f "$jump" "$target" >/dev/null 2>&1 || true
    docker network rm "$network" >/dev/null 2>&1 || true
    rm -rf -- "$work"
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

cat >"$work/Dockerfile" <<'DOCKERFILE'
FROM ubuntu:24.04
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates git openssh-client openssh-server \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --shell /bin/bash test \
 && install -d -m 0755 /run/sshd \
 && install -d -m 0700 -o test -g test /home/test/.ssh
RUN printf '%s\n' \
      'PasswordAuthentication no' \
      'KbdInteractiveAuthentication no' \
      'PermitRootLogin no' \
      'AllowAgentForwarding yes' \
      'AllowTcpForwarding yes' \
      > /etc/ssh/sshd_config.d/ghis-smoke.conf
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
DOCKERFILE

docker build --quiet --tag ghis-ssh-smoke:local "$work" >/dev/null
docker network create "$network" >/dev/null
docker run --detach --name "$jump" --network "$network" \
    --network-alias jump -p 127.0.0.1::22 ghis-ssh-smoke:local >/dev/null
docker run --detach --name "$target" --network "$network" \
    --network-alias target ghis-ssh-smoke:local >/dev/null

ssh-keygen -q -t ed25519 -N '' -f "$work/login"
ssh-keygen -q -t ed25519 -N '' -f "$work/signing"
for container in "$jump" "$target"; do
    docker cp "$work/login.pub" "$container:/home/test/.ssh/authorized_keys"
    docker exec "$container" chown test:test /home/test/.ssh/authorized_keys
    docker exec "$container" chmod 600 /home/test/.ssh/authorized_keys
done

docker cp "$project_dir/target/release/ghis" "$target:/usr/local/bin/ghis"
docker exec "$target" chmod 755 /usr/local/bin/ghis
docker cp "$work/signing.pub" "$target:/home/test/signing.pub"
docker exec "$target" chown test:test /home/test/signing.pub

jump_port=$(docker port "$jump" 22/tcp | awk -F: 'NR == 1 { print $NF }')
cat >"$work/ssh_config" <<EOF
Host jump
  HostName 127.0.0.1
  Port $jump_port
  User test
  IdentityFile $work/login
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  LogLevel ERROR

Host target target-no-agent
  HostName target
  User test
  ProxyJump jump
  IdentityFile $work/login
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  LogLevel ERROR

Host target
  ForwardAgent yes

Host target-no-agent
  ForwardAgent no
EOF

for _ in $(seq 1 30); do
    if ssh -F "$work/ssh_config" jump true 2>/dev/null; then
        break
    fi
    sleep 1
done
ssh -F "$work/ssh_config" jump true
ssh -F "$work/ssh_config" target-no-agent true

eval "$(ssh-agent -s)" >/dev/null
agent_pid=$SSH_AGENT_PID
ssh-add "$work/signing" >/dev/null
fingerprint=$(ssh-keygen -lf "$work/signing.pub" -E sha256 | awk '{print $2}')

write_config() {
    destination=$1
    selected_fingerprint=$2
    program=${3:-}
    {
        cat <<EOF
version = 1

[profiles.work]
host = "github.com"
login = "worker"
git_name = "Work Identity"
git_email = "work@example.test"

[profiles.work.signing]
enabled = true
transport = "forwarded-agent"
signing_key = "/home/test/signing.pub"
fingerprint = "$selected_fingerprint"
EOF
        if [ -n "$program" ]; then
            printf 'program = "%s"\n' "$program"
        fi
    } | docker exec -i "$target" sh -c "cat > '$destination' && chown test:test '$destination'"
}

write_config /home/test/ghis.toml "$fingerprint"
write_config /home/test/mismatch.toml SHA256:not-the-forwarded-key
write_config /home/test/missing-signer.toml "$fingerprint" /missing/ssh-keygen

ssh -F "$work/ssh_config" target '
    set -eu
    test -n "${SSH_AUTH_SOCK:-}"
    ssh-add -L | grep -Fq "ssh-ed25519"
    mkdir -p /home/test/repo
    cd /home/test/repo
    git init -q
    /usr/local/bin/ghis --config /home/test/ghis.toml use work --repo /home/test/repo
    /usr/local/bin/ghis --config /home/test/ghis.toml git -- commit --allow-empty -m signed
    git cat-file -p HEAD | grep -Fq "BEGIN SSH SIGNATURE"
'

ssh -F "$work/ssh_config" target-no-agent '
    set -eu
    test -z "${SSH_AUTH_SOCK:-}"
    cd /home/test/repo
    if /usr/local/bin/ghis --config /home/test/ghis.toml git -- commit --allow-empty -m no-agent; then
        echo "forwarded signing succeeded without ForwardAgent" >&2
        exit 90
    fi
'

ssh -F "$work/ssh_config" target '
    set -eu
    cd /home/test/repo
    if SSH_AUTH_SOCK=/missing/socket /usr/local/bin/ghis --config /home/test/ghis.toml git -- commit --allow-empty -m bad-socket; then
        echo "forwarded signing accepted an inaccessible socket" >&2
        exit 91
    fi
    if /usr/local/bin/ghis --config /home/test/mismatch.toml git -- commit --allow-empty -m mismatch; then
        echo "forwarded signing accepted a mismatched fingerprint" >&2
        exit 92
    fi
    if /usr/local/bin/ghis --config /home/test/missing-signer.toml git -- commit --allow-empty -m missing-signer; then
        echo "forwarded signing accepted a missing signer" >&2
        exit 93
    fi
'

docker exec "$jump" sh -c "printf '%s\n' 'AllowTcpForwarding no' >> /etc/ssh/sshd_config.d/ghis-smoke.conf && kill -HUP 1"
sleep 1
if ssh -F "$work/ssh_config" target true 2>/dev/null; then
    echo "ProxyJump succeeded after the jump host disabled TCP forwarding" >&2
    exit 94
fi

printf '%s\n' 'real ProxyJump and forwarded-agent signing smoke passed'
