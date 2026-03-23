#!/usr/bin/env bash
set -euo pipefail

INSTANCE_NAME="${1:-ubuntu-2404}"
LIMA_FILE="$(mktemp -t "${INSTANCE_NAME}.XXXXXX.yaml")"

cleanup() {
  rm -f "$LIMA_FILE"
}
trap cleanup EXIT

if limactl list 2>/dev/null | awk 'NR>1 {print $1}' | grep -Fxq "$INSTANCE_NAME"; then
  limactl delete -f "$INSTANCE_NAME"
fi

cat > "$LIMA_FILE" <<'EOF'
images:
  - location: "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-amd64.img"
    arch: "x86_64"
  - location: "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-arm64.img"
    arch: "aarch64"

cpus: 4
memory: "8GiB"
disk: "40GiB"

mounts:
  - location: "~"
    writable: true

provision:
  - mode: system
    script: |
      #!/bin/bash
      set -euxo pipefail

      if ! id -u ubuntu >/dev/null 2>&1; then
        useradd -m -s /bin/bash ubuntu
      fi

      usermod -aG sudo ubuntu

      mkdir -p /home/ubuntu/.ssh
      if [ -f /home/lima/.ssh/authorized_keys ]; then
        cp -a /home/lima/.ssh/authorized_keys /home/ubuntu/.ssh/authorized_keys
      fi
      chown -R ubuntu:ubuntu /home/ubuntu/.ssh
      chmod 700 /home/ubuntu/.ssh
      if [ -f /home/ubuntu/.ssh/authorized_keys ]; then
        chmod 600 /home/ubuntu/.ssh/authorized_keys
      fi

      echo 'ubuntu ALL=(ALL) NOPASSWD:ALL' >/etc/sudoers.d/99-ubuntu
      chmod 440 /etc/sudoers.d/99-ubuntu
EOF

limactl start -y --name="$INSTANCE_NAME" "$LIMA_FILE"
exec limactl shell "$INSTANCE_NAME" sudo -iu ubuntu
