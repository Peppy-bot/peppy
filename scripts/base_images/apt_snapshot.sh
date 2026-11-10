#!/bin/sh
set -eu

# Makes apt in a peppy base image read one dated snapshot of the Ubuntu archive
# (https://snapshot.ubuntu.com) in place of the regular mirrors.
#
#   apt_snapshot.sh setup <snapshot id> [<package>...]
#   apt_snapshot.sh probe <snapshot id>
#
# A snapshot id is a UTC time in the form YYYYMMDDTHHMMSSZ. The index and the
# package files of a snapshot do not change, so they agree at all times. The
# regular mirrors (archive.ubuntu.com, security.ubuntu.com, ports.ubuntu.com)
# are each a set of hosts that are not in the same state for some hours after
# Ubuntu publishes an update: an index from one host and a package file from
# another fail an install with HTTP 404.
#
# `setup` runs in the build of a base image, as the one step that touches apt.
# It names the snapshot as the only source of apt, brings every package of the
# image to its version in the snapshot, installs the packages it is given, and
# stops the build unless the image passes the checks of `verify_image` below.
# A build on top of the image then reads the same snapshot, so it installs the
# same packages each time and upgrades no package of the image.
#
# `probe` runs in a container of a published base image. It repeats the checks
# of `setup` and installs one package, which proves that the snapshot serves
# its package files. The release scripts run it with the names of the regular
# mirrors resolved to an address that does not answer (see
# scripts/functions/docker.py).

SNAPSHOT_ARCHIVE="https://snapshot.ubuntu.com/ubuntu"
SOURCES_FILE="/etc/apt/sources.list.d/ubuntu.sources"
APT_CONF_FILE="/etc/apt/apt.conf.d/90peppy-apt-snapshot"

# A small package of `universe` that no base image holds.
PROBE_PACKAGE="hello"

fail() {
    echo "apt_snapshot.sh: error: $*" >&2
    exit 1
}

usage() {
    fail "usage: apt_snapshot.sh setup <snapshot id> [<package>...] | apt_snapshot.sh probe <snapshot id>"
}

require_snapshot_id() {
    if ! printf '%s\n' "$1" | grep -Eqx '[0-9]{8}T[0-9]{6}Z'; then
        fail "'$1' is not a snapshot id: give a UTC time as YYYYMMDDTHHMMSSZ"
    fi
}

snapshot_uri() {
    printf '%s/%s/' "$SNAPSHOT_ARCHIVE" "$1"
}

# One stanza for both architectures: the snapshot holds the packages of amd64
# and of arm64 under the same path.
write_sources() {
    # shellcheck source=/dev/null
    . /etc/os-release
    cat > "$SOURCES_FILE" <<EOF
Types: deb
URIs: $(snapshot_uri "$1")
Suites: ${VERSION_CODENAME} ${VERSION_CODENAME}-updates ${VERSION_CODENAME}-backports ${VERSION_CODENAME}-security
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
}

write_apt_conf() {
    cat > "$APT_CONF_FILE" <<'EOF'
// Written by scripts/base_images/apt_snapshot.sh of peppy.

// One service, snapshot.ubuntu.com, is behind every install, so apt tries a
// download again, three times, after a timeout or a refused connection. apt
// does not try a download again after an HTTP error status (4xx, 5xx).
Acquire::Retries "3";

// A phased update of Ubuntu reaches a share of the machines, which apt picks
// from the machine id. Every build reads the newest version of the snapshot,
// with or without a machine id.
APT::Get::Always-Include-Phased-Updates "true";
EOF
}

# The snapshot service answers over HTTPS only, and the Ubuntu image has no CA
# certificates. This one install reads the snapshot without a check of the
# certificate of the server. apt still checks the signature of the index
# against the Ubuntu archive key of the image, and each package against the
# index, as it does for the regular mirrors over HTTP.
install_ca_certificates() {
    no_peer_check="-o Acquire::https::Verify-Peer=false -o Acquire::https::Verify-Host=false"
    # shellcheck disable=SC2086
    apt-get $no_peer_check update
    # shellcheck disable=SC2086
    apt-get $no_peer_check install -y --no-install-recommends ca-certificates
}

# Stops unless every index that apt reads is under the snapshot. apt names
# the indexes of all its sources without a download, one per line, as
# `'<uri>' <file name> <size>`. The path of an index starts with `dists/`
# under the URI of its repository.
require_snapshot_is_the_only_source() {
    expected=$(snapshot_uri "$1")
    index_uris=$(apt-get update --print-uris)
    [ -n "$index_uris" ] || fail "apt has no source"

    other_repositories=""
    while read -r quoted_uri _; do
        uri=${quoted_uri#\'}
        uri=${uri%\'}
        repository=${uri%%dists/*}
        if [ "$repository" = "$expected" ]; then
            continue
        fi
        case "$other_repositories" in
            *"
  ${repository}"*) ;;
            *) other_repositories="${other_repositories}
  ${repository}" ;;
        esac
    done <<EOF
$index_uris
EOF

    [ -z "$other_repositories" ] || fail "apt reads a source other than the snapshot $1:${other_repositories}"
}

# Stops unless every installed package is at its version in the snapshot.
require_no_pending_upgrade() {
    simulated=$(apt-get --simulate dist-upgrade)
    pending=$(printf '%s\n' "$simulated" | grep '^Inst ' || true)
    [ -z "$pending" ] || fail "the snapshot $1 upgrades packages of the image:
${pending}"
}

# Leaves the index of the snapshot in place.
verify_image() {
    require_snapshot_is_the_only_source "$1"
    apt-get update
    require_no_pending_upgrade "$1"
}

setup() {
    snapshot_id=$1
    shift

    write_sources "$snapshot_id"
    write_apt_conf
    install_ca_certificates
    apt-get dist-upgrade -y
    if [ "$#" -gt 0 ]; then
        apt-get install -y --no-install-recommends "$@"
    fi
    verify_image "$snapshot_id"
    rm -rf /var/lib/apt/lists/*
}

probe() {
    verify_image "$1"
    apt-get install -y --no-install-recommends "$PROBE_PACKAGE"
}

[ "$#" -ge 2 ] || usage
mode=$1
snapshot_id=$2
shift 2
require_snapshot_id "$snapshot_id"

case "$mode" in
    setup) setup "$snapshot_id" "$@" ;;
    probe)
        [ "$#" -eq 0 ] || usage
        probe "$snapshot_id"
        ;;
    *) usage ;;
esac
