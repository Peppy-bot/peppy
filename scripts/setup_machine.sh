#!/usr/bin/env bash
set -euo pipefail

# Set up a new development machine for PeppyOS.
#
# Installs each of the following with its recommended method, skipping anything
# that is already on the system:
#   - qemu
#   - Go
#   - pixi
#   - uv
#   - Lima (macOS only)
#
# Supported platforms: Ubuntu (apt) and macOS (Homebrew).
#
# Usage:
#   ./scripts/setup_machine.sh

usage() {
    cat <<'EOF'
Usage: ./scripts/setup_machine.sh

Installs qemu, Go, pixi, uv, and (on macOS) Lima, skipping anything already
present. Supported platforms: Ubuntu and macOS.
EOF
}

case "${1:-}" in
-h | --help)
    usage
    exit 0
    ;;
"") ;;
*)
    echo "error: unexpected argument '$1'" >&2
    usage >&2
    exit 1
    ;;
esac

# --- output helpers ---------------------------------------------------------

if [ -t 1 ]; then
    CYAN="$(printf '\033[1;36m')"
    GREEN="$(printf '\033[1;32m')"
    YELLOW="$(printf '\033[1;33m')"
    RED="$(printf '\033[1;31m')"
    RESET="$(printf '\033[0m')"
else
    CYAN="" GREEN="" YELLOW="" RED="" RESET=""
fi

log() { printf '%s==>%s %s\n' "$CYAN" "$RESET" "$*"; }
skip() { printf '%s ok %s %s already installed (%s)\n' "$GREEN" "$RESET" "$1" "$2"; }
warn() { printf '%swarn%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die() {
    printf '%serror:%s %s\n' "$RED" "$RESET" "$*" >&2
    exit 1
}

have() { command -v "$1" >/dev/null 2>&1; }

# --- platform detection -----------------------------------------------------

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$ARCH" in
x86_64 | amd64) GO_ARCH="amd64" ;;
aarch64 | arm64) GO_ARCH="arm64" ;;
*) GO_ARCH="" ;; # validated only if we need the Go tarball
esac

case "$OS" in
Linux)
    have apt-get || die "Linux support targets Ubuntu (apt); 'apt-get' was not found"
    PLATFORM="ubuntu"
    ;;
Darwin)
    PLATFORM="macos"
    ;;
*)
    die "unsupported operating system '$OS' (supported: Ubuntu, macOS)"
    ;;
esac

# Make freshly installed tools visible within this run so detection and the
# final summary are accurate before the user reopens their shell.
export PATH="/usr/local/go/bin:$HOME/.pixi/bin:$HOME/.local/bin:$PATH"

# Resolve how we elevate for apt and writing to /usr/local on Ubuntu. When the
# script is run without sudo, prompt for the password upfront so a later apt or
# tarball step does not stall waiting for input.
SUDO=""
if [ "$PLATFORM" = "ubuntu" ] && [ "$(id -u)" -ne 0 ]; then
    have sudo || die "root privileges are required (apt, /usr/local); install sudo or run as root"
    SUDO="sudo"
    log "Requesting sudo access (needed for apt and /usr/local)"
    sudo -v
fi

# Homebrew is the recommended source for qemu, Go, and Lima on macOS.
if [ "$PLATFORM" = "macos" ] && ! have brew; then
    die "Homebrew is required on macOS but was not found.
Install it from https://brew.sh, then re-run this script:
  /bin/bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\""
fi

APT_UPDATED=false
apt_install() {
    if ! $APT_UPDATED; then
        log "Refreshing apt package index"
        $SUDO apt-get update -y
        APT_UPDATED=true
    fi
    $SUDO apt-get install -y "$@"
}

# Append a PATH line to a profile file, but only once.
ensure_path_line() {
    local file="$1" line="$2"
    [ -f "$file" ] || touch "$file"
    if ! grep -Fqs "$line" "$file"; then
        printf '\n# Added by PeppyOS setup_machine.sh\n%s\n' "$line" >>"$file"
        warn "Added Go to PATH in $file; run 'source $file' or open a new shell to pick it up"
    fi
}

# --- installers -------------------------------------------------------------

install_qemu() {
    if have qemu-system-x86_64 || have qemu-system-aarch64 || have qemu-img; then
        skip "qemu" "$(command -v qemu-img || command -v qemu-system-x86_64 || command -v qemu-system-aarch64)"
        return
    fi
    log "Installing qemu"
    case "$PLATFORM" in
    ubuntu) apt_install qemu-system qemu-utils ;;
    macos) brew install qemu ;;
    esac
}

install_go() {
    if have go || [ -x /usr/local/go/bin/go ]; then
        skip "Go" "$(command -v go || echo /usr/local/go/bin/go)"
        return
    fi
    log "Installing Go"
    if [ "$PLATFORM" = "macos" ]; then
        brew install go
        return
    fi

    # Ubuntu: install the current release from go.dev (the method go.dev
    # recommends), since apt's packaged Go lags several minor versions.
    [ -n "$GO_ARCH" ] || die "unsupported architecture '$ARCH' for the Go tarball"
    local version url tmp
    version="$(curl -fsSL 'https://go.dev/VERSION?m=text' | head -n1)"
    [ -n "$version" ] || die "could not determine the latest Go version from go.dev"
    url="https://go.dev/dl/${version}.linux-${GO_ARCH}.tar.gz"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    log "Downloading ${version} (linux-${GO_ARCH})"
    curl -fsSL "$url" -o "$tmp/go.tar.gz"
    $SUDO rm -rf /usr/local/go
    $SUDO tar -C /usr/local -xzf "$tmp/go.tar.gz"
    ensure_path_line "$HOME/.profile" 'export PATH=$PATH:/usr/local/go/bin'
}

install_pixi() {
    if have pixi || [ -x "$HOME/.pixi/bin/pixi" ]; then
        skip "pixi" "$(command -v pixi || echo "$HOME/.pixi/bin/pixi")"
        return
    fi
    log "Installing pixi"
    curl -fsSL https://pixi.sh/install.sh | sh
}

install_uv() {
    if have uv || [ -x "$HOME/.local/bin/uv" ]; then
        skip "uv" "$(command -v uv || echo "$HOME/.local/bin/uv")"
        return
    fi
    log "Installing uv"
    curl -LsSf https://astral.sh/uv/install.sh | sh
}

install_lima() {
    if [ "$PLATFORM" != "macos" ]; then
        return # Lima is requested on macOS only
    fi
    if have limactl; then
        skip "Lima" "$(command -v limactl)"
        return
    fi
    log "Installing Lima"
    brew install lima
}

# --- run --------------------------------------------------------------------

log "Setting up a new machine for PeppyOS (${PLATFORM}/${ARCH})"
install_qemu
install_go
install_pixi
install_uv
install_lima
log "Done. Open a new shell (or source your profile) so freshly installed tools are on PATH."
