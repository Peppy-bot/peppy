#!/bin/sh
set -eu

# Install peppy from peppy.bot releases.
# Usage:
#   curl -fsSL https://peppy.bot/install.sh | sh
#   curl -fsSL https://peppy.bot/install.sh | sh -s -- ./peppy-x86_64-unknown-linux-gnu.tgz
#   ./install.sh ./peppy-x86_64-unknown-linux-gnu.tgz
#
# Environment variables:
#   PEPPY_VERSION           Version to install (default: latest)
#   PEPPY_HOME              Install prefix (default: ~/.peppy)
#   PEPPY_BIN_DIR           Binary install directory (default: $PEPPY_HOME/bin)
#   PEPPY_PLATFORM          Linux target suffix (default: auto-detected)
#   PEPPY_ARCH              Override detected architecture (e.g. aarch64, x86_64)
#   PEPPY_REPOURL           Base URL for downloads (default: https://peppy.bot)
#   PEPPY_DOWNLOAD_URL      Override full download URL
#   PEPPY_NO_PATH_UPDATE    If set, do not update shell PATH config
#   PEPPY_FORCE_REINSTALL   If set, skip confirmation when daemon is running (for non-interactive installs)

__wrap__() {
    IS_TTY=false
    if [ -t 1 ]; then
        IS_TTY=true
    fi

    BOLD=""
    DIM=""
    GREEN=""
    CYAN=""
    ORANGE=""
    RESET=""
    if $IS_TTY; then
        BOLD="$(printf '\033[1m')"
        DIM="$(printf '\033[2m')"
        GREEN="$(printf '\033[32m')"
        CYAN="$(printf '\033[36m')"
        ORANGE="$(printf '\033[33m')"
        case "${TERM:-}" in
        *256color* | *24bit* | *truecolor*) ORANGE="$(printf '\033[38;5;208m')" ;;
        esac
        RESET="$(printf '\033[0m')"
    fi

    UTF8=false
    case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in
    *UTF-8* | *utf8* | *UTF8*) UTF8=true ;;
    esac

    PROGRESS_FULL="#"
    PROGRESS_EMPTY="-"
    OK_MARK="OK"
    if $UTF8; then
        PROGRESS_FULL="■"
        PROGRESS_EMPTY="·"
        OK_MARK="✓"
    fi
    PROGRESS_WIDTH=54
    PROGRESS_NEEDS_NEWLINE=false

    repeat_char() {
        COUNT="$1"
        CHAR="$2"
        OUT=""
        I=0
        while [ "$I" -lt "$COUNT" ]; do
            OUT="${OUT}${CHAR}"
            I=$((I + 1))
        done
        printf "%s" "$OUT"
    }

    render_progress() {
        PCT="$1"
        LABEL="${2:-}"
        FILLED=$((PCT * PROGRESS_WIDTH / 100))
        EMPTY=$((PROGRESS_WIDTH - FILLED))
        FILLED_BAR="$(repeat_char "$FILLED" "$PROGRESS_FULL")"
        EMPTY_BAR="$(repeat_char "$EMPTY" "$PROGRESS_EMPTY")"

        if $IS_TTY; then
            printf "\r%s%s%s %3s%%" "$ORANGE" "${FILLED_BAR}${EMPTY_BAR}" "$RESET" "$PCT"
            if [ -n "$LABEL" ]; then
                printf " %s" "$LABEL"
            fi
            # Clear any trailing characters from a previous, longer progress label.
            printf "\033[K"
            PROGRESS_NEEDS_NEWLINE=true
            if [ "$PCT" -ge 100 ]; then
                printf "\n"
                PROGRESS_NEEDS_NEWLINE=false
            fi
        else
            if [ -n "$LABEL" ]; then
                printf "[%3s%%] %s\n" "$PCT" "$LABEL"
            else
                printf "[%3s%%]\n" "$PCT"
            fi
        fi
    }

    flush_progress_line() {
        if $IS_TTY && $PROGRESS_NEEDS_NEWLINE; then
            printf "\n"
            PROGRESS_NEEDS_NEWLINE=false
        fi
    }

    print_banner() {
        if $IS_TTY; then
            printf "\n%s" "$CYAN"
        else
            printf "\n"
        fi
        cat <<'EOF'
██████╗ ███████╗██████╗ ██████╗ ██╗   ██╗ ██████╗ ███████╗
██╔══██╗██╔════╝██╔══██╗██╔══██╗╚██╗ ██╔╝██╔═══██╗██╔════╝
██████╔╝█████╗  ██████╔╝██████╔╝ ╚████╔╝ ██║   ██║███████╗
██╔═══╝ ██╔══╝  ██╔═══╝ ██╔═══╝   ╚██╔╝  ██║   ██║╚════██║
██║     ███████╗██║     ██║        ██║   ╚██████╔╝███████║
╚═╝     ╚══════╝╚═╝     ╚═╝        ╚═╝    ╚═════╝ ╚══════╝
EOF
        if $IS_TTY; then
            printf "%s\n" "$RESET"
        fi
    }

    mask_credentials() {
        URL="$1"
        echo "$URL" | sed -E 's|://[^:@/]+:[^@/]+@|://***:***@|g'
    }

    ARCHIVE_PATH="${1:-}"
    if [ -n "${ARCHIVE_PATH-}" ]; then
        if [ -n "${2:-}" ]; then
            echo "error: unexpected extra argument '${2}'" >&2
            exit 1
        fi
        case "$ARCHIVE_PATH" in
        -h | --help)
            echo "Usage:"
            echo "  ./install.sh [https://peppy.bot/v0.1.0/peppy-<arch>-<platform>.tgz]"
            echo "  curl -fsSL https://peppy.bot/install.sh | sh -s -- [path/to/archive.tgz]"
            exit 0
            ;;
        '~' | '~'/*) ARCHIVE_PATH="${HOME-}${ARCHIVE_PATH#\~}" ;; # expand tilde
        esac
        if [ ! -f "$ARCHIVE_PATH" ]; then
            echo "error: archive not found: $ARCHIVE_PATH" >&2
            exit 1
        fi
        if [ ! -r "$ARCHIVE_PATH" ]; then
            echo "error: archive not readable: $ARCHIVE_PATH" >&2
            exit 1
        fi
    fi

    VERSION="${PEPPY_VERSION:-latest}"
    PEPPY_HOME="${PEPPY_HOME:-$HOME/.peppy}"
    case "$PEPPY_HOME" in
    '~' | '~'/*) PEPPY_HOME="${HOME-}${PEPPY_HOME#\~}" ;; # expand tilde
    esac
    PEPPY_BIN_DIR="${PEPPY_BIN_DIR:-$PEPPY_HOME/bin}"

    # Detect running daemon and warn before overwriting
    DAEMON_RUNNING=false
    if command -v pgrep >/dev/null 2>&1; then
        if pgrep -x peppy >/dev/null 2>&1 || pgrep -x zenohd >/dev/null 2>&1; then
            DAEMON_RUNNING=true
        fi
    elif command -v ps >/dev/null 2>&1; then
        if ps -e -o comm= 2>/dev/null | grep -qxE 'peppy|zenohd'; then
            DAEMON_RUNNING=true
        fi
    fi

    if $DAEMON_RUNNING; then
        echo ""
        echo "warning: The peppy daemon is currently running."
        echo "         Installing will stop the daemon and wipe '${PEPPY_HOME}' before proceeding."
        echo ""

        if [ -t 0 ] || [ -e /dev/tty ]; then
            printf "Do you want to continue? [y/N] "
            read -r REPLY </dev/tty
            case "$REPLY" in
            [Yy] | [Yy][Ee][Ss]) ;;
            *)
                echo "Installation aborted."
                exit 0
                ;;
            esac
        elif [ -z "${PEPPY_FORCE_REINSTALL:-}" ]; then
            echo "error: cannot prompt for confirmation (no terminal available)." >&2
            echo "       Set PEPPY_FORCE_REINSTALL=1 to skip this check." >&2
            exit 1
        fi

        echo "Stopping peppy daemon..."
        if [ -x "$PEPPY_BIN_DIR/peppy" ]; then
            "$PEPPY_BIN_DIR/peppy" service stop >/dev/null 2>&1 || true
            "$PEPPY_BIN_DIR/peppy" service uninstall >/dev/null 2>&1 || true
        fi
        echo "Removing '${PEPPY_HOME}'..."
        rm -rf "$PEPPY_HOME"
    fi

    REPOURL="${PEPPY_REPOURL:-https://peppy.bot}"
    PLATFORM="$(uname -s)"
    ARCH="${PEPPY_ARCH:-$(uname -m)}"

    detect_linux_platform() {
        # Check if musl libc is in use by examining ldd output
        if command -v ldd >/dev/null 2>&1; then
            if ldd --version 2>&1 | grep -qi musl; then
                echo "unknown-linux-musl"
                return
            fi
        fi
        # Default to glibc
        echo "unknown-linux-gnu"
    }

    if [ "${PLATFORM-}" = "Darwin" ]; then
        PLATFORM="apple-darwin"
    elif [ "${PLATFORM-}" = "Linux" ]; then
        PLATFORM="${PEPPY_PLATFORM:-$(detect_linux_platform)}"
    else
        echo "error: unsupported platform '$PLATFORM' (only macOS and Linux are supported)" >&2
        exit 1
    fi

    case "${ARCH-}" in
    arm64 | aarch64) ARCH="aarch64" ;;
    x86_64 | amd64) ARCH="x86_64" ;;
    esac

    if [ "$PLATFORM" = "apple-darwin" ] && [ "$ARCH" != "aarch64" ]; then
        echo "error: macOS is supported only on Apple Silicon (aarch64/arm64)" >&2
        exit 1
    fi

    # ---- Linux system dependency checks (consolidated) ----------------------
    # Detect all issues first, then offer a single sudo prompt to fix them all.
    if [ "$PLATFORM" != "apple-darwin" ]; then
        SUDO_FIXES=""
        SUDO_FIX_LABELS=""

        # Check 1: unprivileged user namespaces
        if [ -f /proc/sys/kernel/unprivileged_userns_clone ] && \
           [ "$(cat /proc/sys/kernel/unprivileged_userns_clone)" = "0" ]; then
            SUDO_FIXES="${SUDO_FIXES}sysctl -w kernel.unprivileged_userns_clone=1 && "
            SUDO_FIXES="${SUDO_FIXES}mkdir -p /etc/sysctl.d && "
            SUDO_FIXES="${SUDO_FIXES}printf 'kernel.unprivileged_userns_clone=1\n' >> /etc/sysctl.d/99-peppy-userns.conf && "
            SUDO_FIX_LABELS="${SUDO_FIX_LABELS}  - Enable unprivileged user namespaces\n"
        fi

        # Check 2: AppArmor user namespace restriction (Ubuntu 24.04+)
        # Install a per-binary AppArmor profile that grants only Apptainer's
        # starter binary the userns permission.
        if [ -f /proc/sys/kernel/apparmor_restrict_unprivileged_userns ] && \
           [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns)" = "1" ] && \
           [ ! -f /etc/apparmor.d/peppy-apptainer ]; then
            STARTER_PATH="$PEPPY_BIN_DIR/apptainer/$(uname -m)/libexec/apptainer/libexec/starter"
            SUDO_FIXES="${SUDO_FIXES}printf 'abi <abi/4.0>,\ninclude <tunables/global>\n\nprofile peppy-apptainer ${STARTER_PATH} flags=(unconfined) {\n  userns,\n}\n' > /etc/apparmor.d/peppy-apptainer && "
            SUDO_FIXES="${SUDO_FIXES}apparmor_parser -r /etc/apparmor.d/peppy-apptainer && "
            SUDO_FIX_LABELS="${SUDO_FIX_LABELS}  - Install AppArmor profile for Apptainer user namespaces\n"
        fi

        # Check 3: newuidmap / newgidmap with setuid bit
        UIDMAP_MISSING=false
        if ! command -v newuidmap >/dev/null 2>&1 || ! command -v newgidmap >/dev/null 2>&1; then
            UIDMAP_MISSING=true
        else
            # Found on PATH — verify setuid bit (leading 4 in octal permissions)
            NUIDMAP_PATH="$(command -v newuidmap)"
            NGIDMAP_PATH="$(command -v newgidmap)"
            NUIDMAP_PERMS="$(stat -c '%a' "$NUIDMAP_PATH" 2>/dev/null || stat -f '%Lp' "$NUIDMAP_PATH" 2>/dev/null)"
            NGIDMAP_PERMS="$(stat -c '%a' "$NGIDMAP_PATH" 2>/dev/null || stat -f '%Lp' "$NGIDMAP_PATH" 2>/dev/null)"
            case "$NUIDMAP_PERMS" in 4*) ;; *) UIDMAP_MISSING=true ;; esac
            case "$NGIDMAP_PERMS" in 4*) ;; *) UIDMAP_MISSING=true ;; esac
        fi

        if $UIDMAP_MISSING; then
            if command -v apt-get >/dev/null 2>&1; then
                SUDO_FIXES="${SUDO_FIXES}apt-get update -qq && apt-get install -y -qq uidmap && "
            elif command -v dnf >/dev/null 2>&1; then
                SUDO_FIXES="${SUDO_FIXES}dnf install -y shadow-utils && "
            elif command -v pacman >/dev/null 2>&1; then
                SUDO_FIXES="${SUDO_FIXES}pacman -S --noconfirm shadow && "
            else
                echo "" >&2
                echo "error: newuidmap/newgidmap (from the uidmap package) are required but not found." >&2
                echo "       No supported package manager detected (apt-get, dnf, pacman)." >&2
                echo "       Install them manually and re-run this script." >&2
                echo "" >&2
                exit 1
            fi
            SUDO_FIX_LABELS="${SUDO_FIX_LABELS}  - Install uidmap package (provides newuidmap/newgidmap)\n"
        fi

        # Check 4: dbus-user-session (required for systemctl --user / D-Bus user bus)
        if command -v dpkg >/dev/null 2>&1; then
            if ! dpkg -s dbus-user-session >/dev/null 2>&1; then
                SUDO_FIXES="${SUDO_FIXES}apt-get update -qq && apt-get install -y -qq dbus-user-session && "
                SUDO_FIX_LABELS="${SUDO_FIX_LABELS}  - Install dbus-user-session (required for peppy background service)\n"
            fi
        fi

        # Check 5: loginctl enable-linger (keeps systemd user session alive after logout)
        CURRENT_USER="$(id -un)"
        LINGER_ENABLED=false
        if command -v loginctl >/dev/null 2>&1; then
            LINGER_VAL="$(loginctl show-user "$CURRENT_USER" -p Linger --value 2>/dev/null || echo "no")"
            if [ "$LINGER_VAL" = "yes" ]; then
                LINGER_ENABLED=true
            fi
        fi
        if ! $LINGER_ENABLED; then
            SUDO_FIXES="${SUDO_FIXES}loginctl enable-linger ${CURRENT_USER} && "
            SUDO_FIX_LABELS="${SUDO_FIX_LABELS}  - Enable lingering for user ${CURRENT_USER} (keeps peppy daemon running after logout)\n"
        fi

        # Prompt once for all fixes
        if [ -n "$SUDO_FIXES" ]; then
            echo ""
            echo "peppy requires the following system changes:"
            printf "$SUDO_FIX_LABELS"
            echo ""

            if [ -t 0 ]; then
                printf "Apply these fixes now? (requires sudo) [Y/n] "
                read -r REPLY
                case "$REPLY" in
                [Nn] | [Nn][Oo])
                    echo "" >&2
                    echo "error: peppy cannot run without these system dependencies." >&2
                    echo "       Apply them manually and re-run the installer." >&2
                    exit 1
                    ;;
                esac
            else
                echo "Proceeding automatically (non-interactive mode)."
            fi

            # Strip trailing " && " and execute everything under one sudo invocation
            SUDO_FIXES="${SUDO_FIXES% && }"
            if ! sudo sh -c "$SUDO_FIXES"; then
                echo "" >&2
                echo "error: failed to apply system fixes." >&2
                exit 1
            fi
            echo "System dependencies configured successfully."
        fi
    fi

    if ! command -v tar >/dev/null 2>&1; then
        echo "error: 'tar' is required to install peppy" >&2
        exit 1
    fi

    BINARY="peppy-${ARCH}-${PLATFORM}"
    EXTENSION=".tgz"

    if [ "$VERSION" = "latest" ]; then
        DOWNLOAD_URL="${PEPPY_DOWNLOAD_URL:-${REPOURL%/}/latest/${BINARY}${EXTENSION-}}"
    else
        DOWNLOAD_URL="${PEPPY_DOWNLOAD_URL:-${REPOURL%/}/v${VERSION#v}/${BINARY}${EXTENSION-}}"
    fi

    echo ""
    printf "%sInstalling peppy version:%s %s\n" "$BOLD" "$RESET" "$VERSION"
    if [ -n "${ARCHIVE_PATH-}" ]; then
        printf "%sSource:%s %s\n" "$DIM" "$RESET" "$ARCHIVE_PATH"
    else
        printf "%sSource:%s %s\n" "$DIM" "$RESET" "$(mask_credentials "$DOWNLOAD_URL")"
    fi
    render_progress 5 "Preparing installer"

    TEMP_FILE="$(mktemp "${TMPDIR:-/tmp}/.peppy_install.XXXXXXXX")"
    TEMP_DIR=""

    cleanup() {
        rm -f "$TEMP_FILE"
        if [ -n "${TEMP_DIR-}" ]; then
            rm -rf "$TEMP_DIR"
        fi
    }

    trap cleanup EXIT

    if [ -n "${ARCHIVE_PATH-}" ]; then
        render_progress 25 "Reading local archive"
        cp "$ARCHIVE_PATH" "$TEMP_FILE"
        render_progress 45 "Archive ready"
    else
        render_progress 20 "Downloading release archive"
        HAVE_CURL=false
        HAVE_CURL_8_8_0=false
        if command -v curl >/dev/null 2>&1; then
            if [ "$(curl --version | (
                IFS=' ' read -r _ v _
                printf %s "${v-}"
            ))" = "8.8.0" ]; then
                HAVE_CURL_8_8_0=true
            else
                HAVE_CURL=true
            fi
        fi

        HAVE_WGET=true
        command -v wget >/dev/null 2>&1 || HAVE_WGET=false

        if ! $HAVE_CURL && ! $HAVE_WGET; then
            echo "error: you need either 'curl' or 'wget' installed for this script." >&2
            if $HAVE_CURL_8_8_0; then
                echo "error: curl 8.8.0 is known to be broken, please use a different version" >&2
            fi
            exit 1
        fi

        CURL_OPTIONS="-sS"
        WGET_OPTIONS="--no-verbose"

        if [ -n "${NETRC:-}" ]; then
            CURL_OPTIONS="$CURL_OPTIONS --netrc-file $NETRC"
            WGET_OPTIONS="$WGET_OPTIONS --netrc-file=$NETRC"
        elif [ -f "$HOME/.netrc" ]; then
            CURL_OPTIONS="$CURL_OPTIONS --netrc"
            WGET_OPTIONS="$WGET_OPTIONS --netrc"
        fi

        if $HAVE_CURL; then
            CURL_ERR=0
            HTTP_CODE="$(curl -L $CURL_OPTIONS "$DOWNLOAD_URL" --output "$TEMP_FILE" --write-out "%{http_code}")" || CURL_ERR=$?
            case "$CURL_ERR" in
            35 | 53 | 54 | 59 | 66 | 77)
                if ! $HAVE_WGET; then
                    echo "error: when downloading '$(mask_credentials "$DOWNLOAD_URL")', curl has some local ssl problems with error $CURL_ERR" >&2
                    exit 1
                fi
                ;;
            0)
                if [ "${HTTP_CODE}" -eq 401 ]; then
                    echo "error: authentication failed when downloading '$(mask_credentials "$DOWNLOAD_URL")'" >&2
                    echo "       Check your .netrc file, NETRC environment variable, or hardcoded credentials in PEPPY_DOWNLOAD_URL." >&2
                    exit 1
                elif [ "${HTTP_CODE}" -lt 200 ] || [ "${HTTP_CODE}" -gt 299 ]; then
                    echo "error: '$(mask_credentials "$DOWNLOAD_URL")' is not available (HTTP ${HTTP_CODE})" >&2
                    exit 1
                fi
                HAVE_WGET=false
                ;;
            *)
                echo "error: when downloading '$(mask_credentials "$DOWNLOAD_URL")', curl fails with error $CURL_ERR" >&2
                exit 1
                ;;
            esac
        fi

        if $HAVE_WGET && ! wget $WGET_OPTIONS --output-document="$TEMP_FILE" "$DOWNLOAD_URL"; then
            echo "error: '$(mask_credentials "$DOWNLOAD_URL")' is not available" >&2
            exit 1
        fi
        render_progress 45 "Download complete"
    fi

    if [ ! -s "$TEMP_FILE" ]; then
        echo "error: temporary file ${TEMP_FILE} not correctly created." >&2
        echo "       As a workaround, you can try setting TMPDIR env variable to a directory with write permissions." >&2
        exit 1
    fi

    render_progress 60 "Extracting archive"
    mkdir -p "$PEPPY_HOME"
    mkdir -p "$PEPPY_BIN_DIR"
    TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/.peppy_install_dir.XXXXXXXX")"
    tar -xzf "$TEMP_FILE" -C "$TEMP_DIR"

    if [ ! -f "$TEMP_DIR/bin/peppy" ]; then
        echo "error: could not find 'bin/peppy' in the downloaded archive." >&2
        exit 1
    fi

    # Install apptainer/lima directory trees into PEPPY_BIN_DIR (siblings of the peppy binary)
    for DIR_NAME in apptainer lima; do
        if [ -d "$TEMP_DIR/bin/$DIR_NAME" ]; then
            rm -rf "$PEPPY_BIN_DIR/$DIR_NAME"
            mv "$TEMP_DIR/bin/$DIR_NAME" "$PEPPY_BIN_DIR/$DIR_NAME"
        fi
    done

    # Create lima-data directory for VM instance state (preserved across upgrades)
    if [ -d "$PEPPY_BIN_DIR/lima" ] && [ ! -d "$PEPPY_HOME/lima-data" ]; then
        mkdir -p "$PEPPY_HOME/lima-data"
    fi

    render_progress 75 "Installing binaries"
    # Install binaries
    mv "$TEMP_DIR/bin/peppy" "$PEPPY_BIN_DIR/peppy"
    chmod +x "$PEPPY_BIN_DIR/peppy"

    if [ -f "$TEMP_DIR/bin/zenohd" ]; then
        mv "$TEMP_DIR/bin/zenohd" "$PEPPY_BIN_DIR/zenohd"
        chmod +x "$PEPPY_BIN_DIR/zenohd"
    fi

    if [ "$PEPPY_BIN_DIR" = "$PEPPY_HOME/bin" ]; then
        flush_progress_line
        echo "peppy installed to '${PEPPY_HOME}'"
    else
        flush_progress_line
        echo "peppy installed to '${PEPPY_BIN_DIR}' (PEPPY_HOME=${PEPPY_HOME})"
    fi
    if [ ! -f "$PEPPY_BIN_DIR/zenohd" ]; then
        flush_progress_line
        echo "warning: 'zenohd' was not found in the archive. 'peppy service serve' requires zenohd on PATH or next to the peppy binary." >&2
    fi

    render_progress 85 "Configuring service"
    if [ -z "${PEPPY_NO_SERVICE_INSTALL:-}" ]; then
        # Stop and remove existing service before installing the new one
        "$PEPPY_BIN_DIR/peppy" service stop >/dev/null 2>&1 || true
        "$PEPPY_BIN_DIR/peppy" service uninstall >/dev/null 2>&1 || true

        SERVICE_INSTALL_OUTPUT=""
        if ! SERVICE_INSTALL_OUTPUT=$("$PEPPY_BIN_DIR/peppy" service install 2>&1); then
            flush_progress_line
            case "$SERVICE_INSTALL_OUTPUT" in
            *"Permission denied"* | *"permission denied"* | *"os error 13"*)
                echo "error: failed to install the peppy background service (permission denied; try: 'sudo $PEPPY_BIN_DIR/peppy service install')" >&2
                ;;
            *)
                echo "error: failed to install the peppy background service (try: '$PEPPY_BIN_DIR/peppy service install')" >&2
                ;;
            esac

            if [ -n "${PEPPY_DEBUG:-}" ] && [ -n "${SERVICE_INSTALL_OUTPUT-}" ]; then
                echo "$SERVICE_INSTALL_OUTPUT" >&2
            fi
            exit 1
        fi
    else
        flush_progress_line
        echo "No service install because PEPPY_NO_SERVICE_INSTALL is set"
    fi

    PATH_UPDATED=false
    PATH_ALREADY_PRESENT=false
    PATH_UPDATE_FILE=""

    render_progress 92 "Configuring shell PATH"
    if [ -n "${PEPPY_NO_PATH_UPDATE:-}" ]; then
        flush_progress_line
        echo "No path update because PEPPY_NO_PATH_UPDATE is set"
    else
        update_shell() {
            FILE="$1"
            LINE="$2"

            if [ ! -f "$FILE" ]; then
                touch "$FILE"
            fi

            if ! grep -Fxq "$LINE" "$FILE"; then
                echo >>"$FILE"
                echo "$LINE" >>"$FILE"
                PATH_UPDATED=true
                PATH_UPDATE_FILE="$FILE"
            else
                PATH_ALREADY_PRESENT=true
                PATH_UPDATE_FILE="$FILE"
            fi
        }

        case "$(basename "${SHELL-}")" in
        bash)
            LINE="export PATH=\"${PEPPY_BIN_DIR}:\$PATH\""
            update_shell ~/.bashrc "$LINE"
            ;;

        fish)
            LINE="fish_add_path ${PEPPY_BIN_DIR}"
            update_shell ~/.config/fish/config.fish "$LINE"
            ;;

        zsh)
            LINE="export PATH=\"${PEPPY_BIN_DIR}:\$PATH\""
            update_shell ~/.zshrc "$LINE"
            ;;

        tcsh)
            LINE="set path = ( ${PEPPY_BIN_DIR} \$path )"
            update_shell ~/.tcshrc "$LINE"
            ;;

        '')
            flush_progress_line
            echo "warn: Could not detect shell type." >&2
            echo "      Please permanently add '${PEPPY_BIN_DIR}' to your \$PATH to enable the 'peppy' command." >&2
            ;;

        *)
            flush_progress_line
            echo "warn: Could not update shell $(basename "$SHELL")" >&2
            echo "      Please permanently add '${PEPPY_BIN_DIR}' to your \$PATH to enable the 'peppy' command." >&2
            ;;
        esac
    fi

    render_progress 100 "Installation complete"

    if $PATH_UPDATED; then
        printf "%sSuccessfully added peppy to \$PATH in %s%s\n" "$GREEN" "$PATH_UPDATE_FILE" "$RESET"
    elif $PATH_ALREADY_PRESENT; then
        printf "%speppy is already available in \$PATH via %s%s\n" "$DIM" "$PATH_UPDATE_FILE" "$RESET"
    fi

    print_banner
    echo ""
    printf "%s%s peppy is ready.%s\n\n" "$GREEN" "$OK_MARK" "$RESET"
    echo "To get started, reload your shell and:"
    echo "  peppy         # Run command"
    echo ""
    echo "For more information visit https://docs.peppy.bot"
} && __wrap__ "$@"
