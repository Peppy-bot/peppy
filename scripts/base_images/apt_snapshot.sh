#!/bin/sh
set -eu

# Makes apt in a peppy base image read the Ubuntu archive from one dated
# snapshot (https://snapshot.ubuntu.com), and from the regular Ubuntu mirror
# for a file that the snapshot does not serve.
#
#   apt_snapshot.sh setup <snapshot id> [<package>...]
#   apt_snapshot.sh probe-snapshot <snapshot id>
#   apt_snapshot.sh probe-fallback <snapshot id>
#
# A snapshot id is a UTC time in the form YYYYMMDDTHHMMSSZ. The index and the
# package files of a snapshot do not change, so they agree at all times. The
# regular mirror (archive.ubuntu.com on amd64, ports.ubuntu.com on arm64) is a
# set of hosts that are not in the same state for some hours after Ubuntu
# publishes an update: an index from one host and a package file from another
# fail an install with HTTP 404.
#
# `setup` runs in the build of a base image, as the one step that touches apt.
# It names the snapshot as the only source of apt, brings every package of the
# image to its version in the snapshot, installs the packages it is given, and
# stops the build unless the image passes the checks of `verify_image` below.
# Then it writes the sources of every build on top of the image: a mirror list
# (see apt-transport-mirror(1)) with the snapshot first and the regular mirror
# second. apt asks the snapshot for each file, and the regular mirror for a
# file that the snapshot does not serve. While the snapshot answers, a build
# installs the same packages each time and upgrades no package of the image.
# While it does not, a build installs the packages of the regular mirror.
#
# `probe-snapshot` and `probe-fallback` run in a container of a published base
# image. Each checks the sources that `setup` wrote, updates the index and
# installs one package. The release scripts run `probe-snapshot` with the name
# of the regular mirror resolved to an address that does not answer, so it
# passes only when the snapshot serves every file, and `probe-fallback` with
# the name of the snapshot service resolved to that address, so it passes only
# when the regular mirror serves every file (see scripts/functions/docker.py).

SNAPSHOT_ARCHIVE="https://snapshot.ubuntu.com/ubuntu"

# The tests of the release scripts set APT_ETC_DIR to a directory of their own.
APT_ETC_DIR="${APT_ETC_DIR:-/etc/apt}"
SOURCES_FILE="$APT_ETC_DIR/sources.list.d/ubuntu.sources"
MIRROR_LIST_FILE="$APT_ETC_DIR/peppy-ubuntu-mirrors.txt"
APT_CONF_FILE="$APT_ETC_DIR/apt.conf.d/90peppy-apt-snapshot"

# The URI under which apt names the files of the mirror list.
MIRROR_LIST_URI="mirror+file:$MIRROR_LIST_FILE/"

# A small package of `universe` that no base image holds.
PROBE_PACKAGE="hello"

# The apt-get options of the build of a base image, where apt reads the
# snapshot alone: a download that fails is tried again, three times.
SETUP_RETRIES="-o Acquire::Retries=3"

fail() {
    echo "apt_snapshot.sh: error: $*" >&2
    exit 1
}

usage() {
    fail "usage: apt_snapshot.sh setup <snapshot id> [<package>...] | apt_snapshot.sh probe-snapshot <snapshot id> | apt_snapshot.sh probe-fallback <snapshot id>"
}

require_snapshot_id() {
    if ! printf '%s\n' "$1" | grep -Eqx '[0-9]{8}T[0-9]{6}Z'; then
        fail "'$1' is not a snapshot id: give a UTC time as YYYYMMDDTHHMMSSZ"
    fi
}

snapshot_uri() {
    printf '%s/%s/' "$SNAPSHOT_ARCHIVE" "$1"
}

regular_mirror_uri() {
    architecture=$(dpkg --print-architecture)
    case "$architecture" in
        amd64) printf '%s' "http://archive.ubuntu.com/ubuntu/" ;;
        arm64) printf '%s' "http://ports.ubuntu.com/ubuntu-ports/" ;;
        *) fail "no regular Ubuntu mirror is known for the architecture $architecture" ;;
    esac
}

# The snapshot first, the regular mirror second: apt asks the mirror of the
# lowest priority first.
mirror_list() {
    snapshot=$(snapshot_uri "$1")
    regular_mirror=$(regular_mirror_uri)
    printf '%s\tpriority:1\n%s\tpriority:2\n' "$snapshot" "$regular_mirror"
}

# One stanza for every suite of the release. The snapshot and the mirror list
# hold the packages of amd64 and of arm64 under the same URI.
write_sources() {
    # shellcheck source=/dev/null
    . /etc/os-release
    cat > "$SOURCES_FILE" <<EOF
Types: deb
URIs: $1
Suites: ${VERSION_CODENAME} ${VERSION_CODENAME}-updates ${VERSION_CODENAME}-backports ${VERSION_CODENAME}-security
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
}

write_apt_conf() {
    cat > "$APT_CONF_FILE" <<'EOF'
// Written by scripts/base_images/apt_snapshot.sh of peppy.

// A phased update of Ubuntu reaches a share of the machines, which apt picks
// from the machine id. Every build reads the newest version of the snapshot,
// with or without a machine id.
APT::Get::Always-Include-Phased-Updates "true";

// apt asks the regular mirror for a file that the snapshot does not serve, in
// place of a new try of the snapshot. Without this line apt 2.8 tries a
// download four times, and with more than one try the mirror list makes no
// progress once the snapshot refuses a connection.
Acquire::Retries "0";
EOF
}

# The snapshot service answers over HTTPS only, and the Ubuntu image has no CA
# certificates. This one install reads the snapshot without a check of the
# certificate of the server. apt still checks the signature of the index
# against the Ubuntu archive key of the image, and each package against the
# index, as it does for the regular mirror over HTTP. The update that ends
# this reads the index again with the check of the certificate, so every
# later install works from an index of the real server.
install_ca_certificates() {
    no_peer_check="-o Acquire::https::Verify-Peer=false -o Acquire::https::Verify-Host=false"
    # shellcheck disable=SC2086
    apt-get $SETUP_RETRIES $no_peer_check update --error-on=any
    # shellcheck disable=SC2086
    apt-get $SETUP_RETRIES $no_peer_check install -y --no-install-recommends ca-certificates
    # shellcheck disable=SC2086
    apt-get $SETUP_RETRIES update --error-on=any
}

# Stops unless every index that apt reads is under the repository URI $1.
# apt names the indexes of all its sources without a download, one per line,
# as `'<uri>' <file name> <size>`. The path of an index starts with `dists/`
# under the URI of its repository.
require_only_source() {
    expected=$1
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

    [ -z "$other_repositories" ] || fail "apt reads a source other than ${expected}:${other_repositories}"
}

# Stops unless apt reads the mirror list alone, and the mirror list names the
# snapshot $1 first and the regular mirror second.
require_mirror_list_sources() {
    require_only_source "$MIRROR_LIST_URI"
    expected=$(mirror_list "$1")
    found=$(cat "$MIRROR_LIST_FILE") || fail "$MIRROR_LIST_FILE cannot be read"
    [ "$found" = "$expected" ] || fail "$MIRROR_LIST_FILE does not name the snapshot $1 first and the regular mirror second:
${found}"
}

# Stops unless every installed package is at its version in the index.
require_no_pending_upgrade() {
    simulated=$(apt-get --simulate dist-upgrade)
    pending=$(printf '%s\n' "$simulated" | grep '^Inst ' || true)
    [ -z "$pending" ] || fail "the snapshot $1 upgrades packages of the image:
${pending}"
}

setup() {
    snapshot_id=$1
    shift

    write_sources "$(snapshot_uri "$snapshot_id")"
    write_apt_conf
    install_ca_certificates
    # shellcheck disable=SC2086
    apt-get $SETUP_RETRIES dist-upgrade -y
    if [ "$#" -gt 0 ]; then
        # shellcheck disable=SC2086
        apt-get $SETUP_RETRIES install -y --no-install-recommends "$@"
    fi
    require_only_source "$(snapshot_uri "$snapshot_id")"
    # shellcheck disable=SC2086
    apt-get $SETUP_RETRIES update --error-on=any
    require_no_pending_upgrade "$snapshot_id"

    mirror_list "$snapshot_id" > "$MIRROR_LIST_FILE"
    write_sources "$MIRROR_LIST_URI"
    require_mirror_list_sources "$snapshot_id"
    rm -rf /var/lib/apt/lists/*
}

probe_snapshot() {
    require_mirror_list_sources "$1"
    apt-get update --error-on=any
    require_no_pending_upgrade "$1"
    apt-get install -y --no-install-recommends "$PROBE_PACKAGE"
}

# The regular mirror is newer than the snapshot, so its index can upgrade
# packages of the image.
probe_fallback() {
    require_mirror_list_sources "$1"
    apt-get update --error-on=any
    apt-get install -y --no-install-recommends "$PROBE_PACKAGE"
}

[ "$#" -ge 2 ] || usage
mode=$1
snapshot_id=$2
shift 2
require_snapshot_id "$snapshot_id"

case "$mode" in
    setup) setup "$snapshot_id" "$@" ;;
    probe-snapshot)
        [ "$#" -eq 0 ] || usage
        probe_snapshot "$snapshot_id"
        ;;
    probe-fallback)
        [ "$#" -eq 0 ] || usage
        probe_fallback "$snapshot_id"
        ;;
    *) usage ;;
esac
