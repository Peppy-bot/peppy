#!/usr/bin/env bash

# `sh scripts/setup_machine.sh` bypasses the shebang above, and Ubuntu's sh is
# dash, which has no `pipefail` and cannot parse the arrays below. It reports
# that as `set: Illegal option -o pipefail` on line 2, which says nothing about
# the cause, so the invocation is corrected here rather than diagnosed.
if [ -z "${BASH_VERSION:-}" ]; then
    exec bash "$0" "$@"
fi

set -euo pipefail

# Set up a new machine for Peppy: everything the workspace builds and tests
# against, so a host that has run this can compile peppy, build apptainer from
# source, and run the container suites.
#
# Installs each of the following with its recommended method, skipping anything
# that is already on the system:
#   - the apptainer build dependencies (and fuse2fs, needed at run time)
#   - the Rust toolchain, with clippy
#   - qemu
#   - Go
#   - pixi
#   - uv
#   - Docker (Ubuntu; the multi-daemon suite builds its image with buildx)
#   - Lima (macOS only)
#
# The list is not a matter of taste: every entry is here because its absence
# broke a real build. `g++` because apptainer's `mconfig` probes a host C++
# compiler and Ubuntu's `gcc` package does not pull one in; clippy because the
# generator suites run `cargo clippy -- -D warnings` over the crates they
# generate; Go because containers-internal compiles apptainer with the `go` on
# PATH and only its Lima path bootstraps a toolchain of its own.
#
# Supported platforms: Ubuntu (apt) and macOS (Homebrew).
#
# Usage:
#   ./scripts/setup_machine.sh [--ci-runner]

usage() {
    cat <<'EOF'
Usage: ./scripts/setup_machine.sh [--ci-runner]

Installs the apptainer build dependencies, the Rust toolchain (with clippy),
qemu, Go, pixi, uv, Docker, and (on macOS) Lima, skipping anything already
present. Supported platforms: Ubuntu and macOS.

  --ci-runner   Additionally prepare the host to run this repository's CI as a
                self-hosted GitHub Actions runner: grant the invoking user
                passwordless sudo. The container suites re-install peppy under
                a fresh directory on every job and then run `peppy container
                setup`, which writes an AppArmor profile keyed to that install
                and so needs root without a terminal to prompt at. This grants
                real privilege; read the note it prints before using it.
EOF
}

CI_RUNNER=false
case "${1:-}" in
-h | --help)
    usage
    exit 0
    ;;
--ci-runner)
    CI_RUNNER=true
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
    # Only prompt where a password is actually wanted. `sudo -v` caches
    # credentials and authenticates to do it, and a host that already grants
    # this account passwordless sudo — which every cloud image does for its
    # default user, and which the CI runners need anyway — typically has no
    # password set for it at all. There `sudo -v` prompts for a password that
    # cannot exist and fails, while every real command the script goes on to
    # run succeeds. `sudo -n true` asks the question that matters instead:
    # can we elevate without a prompt?
    if sudo -n true 2>/dev/null; then
        log "Passwordless sudo is already available"
    else
        log "Requesting sudo access (needed for apt and /usr/local)"
        sudo -v
    fi
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
        printf '\n# Added by Peppy setup_machine.sh\n%s\n' "$line" >>"$file"
        warn "Added Go to PATH in $file; run 'source $file' or open a new shell to pick it up"
    fi
}

# --- installers -------------------------------------------------------------

# The packages containers-internal's build script needs to compile apptainer
# and its bundled squashfuse from source. The list mirrors APPTAINER_BUILD_DEPS
# in crates/containers-internal/build.rs, which that build script asserts before
# it starts, plus three the constant does not carry because they are needed to
# run apptainer rather than to build it: fuse2fs, to mount EXT3 images; uidmap,
# which provides the newuidmap/newgidmap that fakeroot needs and whose absence
# `peppy container setup` reports as "Install uidmap package (provides newuidmap
# for fakeroot)" before refusing to continue; and g++, which `mconfig` probes
# for among its base checks and which Ubuntu's `gcc` package does not pull in.
# Keep in step with that constant and with scripts/functions/lima.py.
#
# macOS builds apptainer inside Lima rather than natively, so the guest carries
# these and the host needs none of them.
install_build_deps() {
    if [ "$PLATFORM" != "ubuntu" ]; then
        return
    fi
    local packages=(
        make gcc g++ pkg-config squashfs-tools cryptsetup curl ca-certificates
        libseccomp-dev libfuse3-dev zlib1g-dev liblzo2-dev liblz4-dev
        liblzma-dev libzstd-dev fuse2fs uidmap
    )
    local missing=()
    local package
    for package in "${packages[@]}"; do
        dpkg-query -W -f='${Status}' "$package" 2>/dev/null |
            grep -q '^install ok installed$' || missing+=("$package")
    done
    if [ ${#missing[@]} -eq 0 ]; then
        skip "apptainer build dependencies" "all ${#packages[@]} packages"
        return
    fi
    log "Installing apptainer build dependencies: ${missing[*]}"
    apt_install "${missing[@]}"
}

# rustup rather than a distribution package: the workspace tracks current
# stable (sysinfo alone already requires a newer rustc than the runner images
# this replaced shipped), and a packaged toolchain goes stale in place.
#
# clippy is not optional here. generator-internal's test helpers generate a
# crate and run `cargo clippy --all-targets -- -D warnings` over it, so a
# machine without clippy fails those tests rather than merely skipping a lint.
install_rust() {
    if have rustup || [ -x "$HOME/.cargo/bin/rustup" ]; then
        skip "rustup" "$(command -v rustup || echo "$HOME/.cargo/bin/rustup")"
    else
        log "Installing the Rust toolchain"
        curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs |
            sh -s -- -y --profile minimal --default-toolchain stable
    fi
    export PATH="$HOME/.cargo/bin:$PATH"
    have rustup || die "rustup is still not on PATH after installing it"
    # `--profile minimal` above omits clippy, so it is asked for by name. Both
    # calls are no-ops when they are already satisfied.
    if ! rustup default 2>/dev/null | grep -q '^stable-'; then
        log "Pinning the default toolchain to stable"
        rustup toolchain install stable --profile minimal
        rustup default stable
    fi
    if cargo clippy --version >/dev/null 2>&1; then
        skip "clippy" "$(cargo clippy --version)"
    else
        log "Installing clippy"
        rustup component add clippy
    fi
}

# The multi-daemon end-to-end suite builds its daemon image with `docker buildx
# build` (see build_e2e_daemon_image), falling back to a testcontainers build
# when no buildx client is present. Both paths want a working Docker.
#
# Not installed on macOS: Docker Desktop is a licensed GUI application, so the
# choice of runtime (Desktop, Colima, OrbStack) is the developer's to make.
install_docker() {
    if [ "$PLATFORM" != "ubuntu" ]; then
        if ! have docker; then
            warn "no docker found; install Docker Desktop, Colima or OrbStack for the multi-daemon suite"
        fi
        return
    fi
    if have docker; then
        skip "Docker" "$(command -v docker)"
    else
        log "Installing Docker"
        apt_install docker.io docker-buildx
    fi
    # The daemon's socket is root-owned and group-writable, so membership is
    # what lets the suite talk to it without sudo. A new group does not apply
    # to the current session, hence the note rather than a silent success.
    if [ "$(id -u)" -ne 0 ] && ! id -nG "$(id -un)" | grep -qw docker; then
        log "Adding $(id -un) to the docker group"
        $SUDO usermod -aG docker "$(id -un)"
        warn "docker group membership applies to new logins; log out and back in before running the multi-daemon suite"
    fi
}

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
    # Pinned to the version CI installs (.github/actions/rust-build-env), which
    # satisfies requires-pixi in scripts/pixi.toml and peppylib-py/pixi.toml;
    # bump them together.
    curl -fsSL https://pixi.sh/install.sh | PIXI_VERSION=v0.80.0 sh
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

# Prepare the host to run this repository's CI as a self-hosted runner.
#
# Only one thing here cannot be expressed as a package: passwordless sudo. The
# container suites install a peppy release under the job's own directory and
# then run `peppy container setup`, which writes an AppArmor profile whose file
# name hashes the canonical path of that install's `starter` binary (see
# apparmor_profile in crates/containers-internal/src/apptainer/facade.rs). A
# job has no terminal for sudo to prompt at, so without this the step fails
# with "sudo: a password is required" and every container suite on the box goes
# red — which is exactly how each freshly added runner has announced itself.
#
# The grant is deliberately not silent and not implied by a plain run: it is
# full sudo for the invoking user, on a host that also executes pull request
# code. That is the same trade every CI runner makes, but it should be a
# decision rather than a side effect, hence the flag and this message.
configure_ci_runner() {
    if [ "$PLATFORM" != "ubuntu" ]; then
        die "--ci-runner targets the Ubuntu self-hosted runners; this host is ${PLATFORM}"
    fi
    local user file
    user="$(id -un)"
    if [ "$user" = "root" ]; then
        skip "passwordless sudo" "running as root"
        return
    fi
    file="/etc/sudoers.d/peppy-ci-$user"
    if $SUDO test -f "$file"; then
        skip "passwordless sudo" "$file"
        return
    fi
    log "Granting $user passwordless sudo for CI ($file)"
    warn "this grants $user full root without a password, on a host that runs pull request code"
    # Written through a temporary file and validated before it is installed: a
    # malformed drop-in can lock sudo out of the whole machine, and visudo -c
    # is what catches that while the file is still harmless.
    local tmp
    tmp="$(mktemp)"
    printf '%s ALL=(ALL) NOPASSWD:ALL\n' "$user" >"$tmp"
    if ! $SUDO visudo -c -f "$tmp" >/dev/null; then
        rm -f "$tmp"
        die "refusing to install an invalid sudoers drop-in"
    fi
    $SUDO install -m 0440 -o root -g root "$tmp" "$file"
    rm -f "$tmp"
}

# --- run --------------------------------------------------------------------

log "Setting up a new machine for Peppy (${PLATFORM}/${ARCH})"
install_build_deps
install_rust
install_qemu
install_go
install_pixi
install_uv
install_docker
install_lima
if $CI_RUNNER; then
    configure_ci_runner
fi
log "Done. Open a new shell (or source your profile) so freshly installed tools are on PATH."
