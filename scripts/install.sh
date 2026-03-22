#!/bin/sh
set -eu

# Install peppy from peppy.bot releases.
# Usage:
#   curl -fsSL https://peppy.bot/install.sh | sh
#   curl -fsSL https://peppy.bot/install.sh | sh -s -- ./peppy-x86_64-unknown-linux-gnu.tgz
#   ./install.sh ./peppy-x86_64-unknown-linux-gnu.tgz
#
# Environment variables:
#   PEPPY_VERSION            Version to install (default: latest)
#   PEPPY_HOME               Install prefix (default: ~/.peppy)
#   PEPPY_BIN_DIR            Binary install directory (default: $PEPPY_HOME/bin)
#   PEPPY_PLATFORM           Linux target suffix (default: auto-detected)
#   PEPPY_ARCH               Override detected architecture (e.g. aarch64, x86_64)
#   PEPPY_REPOURL            Base URL for downloads (default: https://peppy.bot)
#   PEPPY_DOWNLOAD_URL       Override full download URL
#   PEPPY_NO_PATH_UPDATE     If set, do not update shell PATH config
#   PEPPY_FORCE_REINSTALL    If set, skip confirmation when daemon is running (for non-interactive installs)
#   PEPPY_NO_ROOT_INSTALL    If set, never use sudo. Missing deps become hard errors; Apptainer setuid setup is deferred to 'peppy container setup'
#   PEPPY_NO_SERVICE_INSTALL If set, skips the installation of the daemon in systemd

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

    # Detect existing installation (daemon may or may not be running)
    EXISTING_INSTALL=false
    if [ -d "$PEPPY_HOME" ]; then
        EXISTING_INSTALL=true
    fi

    if $DAEMON_RUNNING || $EXISTING_INSTALL; then
        echo ""
        if $DAEMON_RUNNING; then
            echo "warning: The peppy daemon is currently running."
            echo "         Installing will stop the daemon and wipe '${PEPPY_HOME}' before proceeding."
        else
            echo "warning: An existing installation was found at '${PEPPY_HOME}'."
            echo "         Installing will wipe this directory before proceeding."
        fi
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

        if $DAEMON_RUNNING; then
            echo "Stopping peppy daemon..."
            if [ -x "$PEPPY_BIN_DIR/peppy" ]; then
                "$PEPPY_BIN_DIR/peppy" service stop >/dev/null 2>&1 || true
                "$PEPPY_BIN_DIR/peppy" service uninstall >/dev/null 2>&1 || true
            fi
        fi
        echo "Removing '${PEPPY_HOME}'..."
        rm -rf "$PEPPY_HOME" 2>/dev/null || sudo rm -rf "$PEPPY_HOME"
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

    # ---- Shared helpers for sudo operations ------------------------------------
    # prompt_sudo_consent: show labels and get user consent (Y/n). Does not execute.
    prompt_sudo_consent() {
        _LABELS="$1"
        echo ""
        echo "peppy requires the following system changes:"
        printf "$_LABELS"
        echo ""

        if [ -t 0 ]; then
            printf "Apply these changes now? (requires sudo) [Y/n] "
            read -r REPLY
            case "$REPLY" in
            [Nn] | [Nn][Oo])
                echo "" >&2
                echo "error: peppy cannot run without these system changes." >&2
                echo "       Apply them manually and re-run the installer." >&2
                exit 1
                ;;
            esac
        else
            echo "Proceeding automatically (non-interactive mode)."
        fi
    }

    # apply_sudo_fixes: execute accumulated fixes under sudo. Consent must
    # already have been obtained. FIXES has trailing " && " stripped internally.
    apply_sudo_fixes() {
        _FIXES="${1% && }"
        _SUCCESS_MSG="$2"

        if [ "$(id -u)" -eq 0 ]; then
            if ! sh -c "$_FIXES"; then
                echo "" >&2
                echo "error: failed to apply system changes." >&2
                exit 1
            fi
        elif command -v sudo >/dev/null 2>&1; then
            if ! sudo sh -c "$_FIXES"; then
                echo "" >&2
                echo "error: failed to apply system changes." >&2
                exit 1
            fi
        else
            echo "" >&2
            echo "error: sudo is required to apply system changes (not running as root)." >&2
            echo "       Either run this script as root or install sudo." >&2
            exit 1
        fi
        echo "$_SUCCESS_MSG"
    }

    # ---- Linux system dependency checks (phase 1: pre-download) ---------------
    # Shows the user a SINGLE comprehensive sudo prompt listing ALL changes
    # (dbus, linger, curl, Apptainer setuid/ownership, AppArmor). Executes
    # pre-download fixes (dbus, linger, curl) immediately; Apptainer commands
    # are deferred to phase 2 post-extraction (files don't exist yet).
    #
    # When PEPPY_NO_ROOT_INSTALL is set, sudo is never used. Missing
    # dependencies become hard errors with manual-install instructions.
    if [ "$PLATFORM" != "apple-darwin" ]; then
        if [ -n "${PEPPY_NO_ROOT_INSTALL:-}" ]; then
            # ---------- no-root mode: verify prerequisites without sudo ----------
            # Check 1: dbus-user-session
            if command -v dpkg >/dev/null 2>&1; then
                if ! dpkg -s dbus-user-session >/dev/null 2>&1; then
                    echo "" >&2
                    echo "error: dbus-user-session is required but not installed." >&2
                    echo "       Install it manually: sudo apt-get install dbus-user-session" >&2
                    echo "" >&2
                    exit 1
                fi
            fi

            # Check 2: loginctl enable-linger
            CURRENT_USER="$(id -un)"
            if command -v loginctl >/dev/null 2>&1; then
                LINGER_VAL="$(loginctl show-user "$CURRENT_USER" -p Linger --value 2>/dev/null || echo "no")"
                if [ "$LINGER_VAL" != "yes" ]; then
                    echo "" >&2
                    echo "error: loginctl linger is not enabled for user ${CURRENT_USER}." >&2
                    echo "       Enable it manually: sudo loginctl enable-linger ${CURRENT_USER}" >&2
                    echo "" >&2
                    exit 1
                fi
            fi

            # Check 3: curl or wget (only when downloading)
            if [ -z "${ARCHIVE_PATH-}" ] && ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
                echo "" >&2
                echo "error: curl or wget is required but not found." >&2
                echo "       Install it manually: sudo apt-get install curl" >&2
                echo "" >&2
                exit 1
            fi
        else
            # ---------- normal mode: prompt ONCE for all sudo changes ---------
            # Gather labels for everything (including predicted Apptainer items)
            # so the user sees the full picture before download begins. Only
            # pre-download fixes (curl) execute now; the rest run post-extraction.
            PREDOWNLOAD_FIXES=""
            ALL_LABELS=""

            # Check 1: dbus-user-session (required for systemctl --user / D-Bus user bus)
            if command -v dpkg >/dev/null 2>&1; then
                if ! dpkg -s dbus-user-session >/dev/null 2>&1; then
                    PREDOWNLOAD_FIXES="${PREDOWNLOAD_FIXES}apt-get update -qq && apt-get install -y -qq dbus-user-session && "
                    ALL_LABELS="${ALL_LABELS}  - Install dbus-user-session (required for peppy background service)\n"
                fi
            fi

            # Check 2: loginctl enable-linger (allows peppy daemon to run after SSH disconnect)
            CURRENT_USER="$(id -un)"
            HAS_LOGINCTL=false
            LINGER_ENABLED=false
            if command -v loginctl >/dev/null 2>&1; then
                HAS_LOGINCTL=true
                LINGER_VAL="$(loginctl show-user "$CURRENT_USER" -p Linger --value 2>/dev/null || echo "no")"
                if [ "$LINGER_VAL" = "yes" ]; then
                    LINGER_ENABLED=true
                fi
            fi
            if $HAS_LOGINCTL && ! $LINGER_ENABLED; then
                PREDOWNLOAD_FIXES="${PREDOWNLOAD_FIXES}loginctl enable-linger ${CURRENT_USER} && "
                ALL_LABELS="${ALL_LABELS}  - Enable systemd linger for user ${CURRENT_USER} (allows peppy daemon to run after SSH disconnect)\n"
            fi

            # Check 3: curl or wget (required to download the release archive)
            if [ -z "${ARCHIVE_PATH-}" ] && ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
                if command -v apt-get >/dev/null 2>&1; then
                    PREDOWNLOAD_FIXES="${PREDOWNLOAD_FIXES}apt-get update -qq && apt-get install -y -qq curl && "
                elif command -v dnf >/dev/null 2>&1; then
                    PREDOWNLOAD_FIXES="${PREDOWNLOAD_FIXES}dnf install -y curl && "
                elif command -v pacman >/dev/null 2>&1; then
                    PREDOWNLOAD_FIXES="${PREDOWNLOAD_FIXES}pacman -S --noconfirm curl && "
                else
                    echo "" >&2
                    echo "error: curl or wget is required but not found." >&2
                    echo "       No supported package manager detected (apt-get, dnf, pacman)." >&2
                    echo "       Install curl or wget manually and re-run this script." >&2
                    echo "" >&2
                    exit 1
                fi
                ALL_LABELS="${ALL_LABELS}  - Install curl (required to download peppy)\n"
            fi

            # Predicted Apptainer labels — the Linux archive always ships with
            # Apptainer, so we can show these before extraction. The actual
            # commands run in phase 2 once the files exist on disk.
            ALL_LABELS="${ALL_LABELS}  - Set setuid permissions on Apptainer starter binary\n"
            ALL_LABELS="${ALL_LABELS}  - Set root ownership on Apptainer configuration\n"
            if [ -f /proc/sys/kernel/apparmor_restrict_unprivileged_userns ] && \
               [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns)" = "1" ]; then
                ALL_LABELS="${ALL_LABELS}  - Install AppArmor profile for Apptainer starter-suid\n"
            fi

            # Single prompt covering everything
            SUDO_CONSENT_GIVEN=false
            if [ -n "$ALL_LABELS" ]; then
                prompt_sudo_consent "$ALL_LABELS"
                SUDO_CONSENT_GIVEN=true
            fi

            # Execute pre-download fixes now (dbus, linger, curl)
            if [ -n "$PREDOWNLOAD_FIXES" ]; then
                apply_sudo_fixes "$PREDOWNLOAD_FIXES" "Pre-download dependencies configured."
            elif $SUDO_CONSENT_GIVEN && [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
                # Prime sudo credential cache so post-extraction doesn't re-prompt
                sudo true
            fi
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
            rm -rf "$PEPPY_BIN_DIR/$DIR_NAME" 2>/dev/null || sudo rm -rf "$PEPPY_BIN_DIR/$DIR_NAME"
            mv "$TEMP_DIR/bin/$DIR_NAME" "$PEPPY_BIN_DIR/$DIR_NAME"
        fi
    done

    # Create lima-data directory for VM instance state (preserved across upgrades)
    if [ -d "$PEPPY_BIN_DIR/lima" ] && [ ! -d "$PEPPY_HOME/lima-data" ]; then
        mkdir -p "$PEPPY_HOME/lima-data"
    fi

    # ---- Linux system dependency checks (phase 2: post-extraction) -----------
    # Apptainer setuid/ownership commands run here because the files only exist
    # after the archive is extracted. User consent was already obtained in
    # phase 1 (pre-download), so no prompt is shown here.
    # Skipped entirely when PEPPY_NO_ROOT_INSTALL is set.
    if [ "$PLATFORM" != "apple-darwin" ] && [ -z "${PEPPY_NO_ROOT_INSTALL:-}" ]; then
        POSTEXTRACT_FIXES=""

        # Apptainer setuid mode requires root ownership on starter-suid
        # (with setuid bit) and the entire etc/apptainer/ config directory.
        STARTER_SUID="$PEPPY_BIN_DIR/apptainer/libexec/apptainer/bin/starter-suid"
        APPTAINER_CONF_DIR="$PEPPY_BIN_DIR/apptainer/etc/apptainer"
        if [ -f "$STARTER_SUID" ]; then
            POSTEXTRACT_FIXES="${POSTEXTRACT_FIXES}chown root:root '$STARTER_SUID' && chmod 4755 '$STARTER_SUID' && "
        fi
        if [ -d "$APPTAINER_CONF_DIR" ]; then
            POSTEXTRACT_FIXES="${POSTEXTRACT_FIXES}chown -R root:root '$APPTAINER_CONF_DIR' && "
        fi

        # AppArmor profile for starter-suid (Ubuntu 24.04+)
        if [ -f /proc/sys/kernel/apparmor_restrict_unprivileged_userns ] && \
           [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns)" = "1" ] && \
           [ -f "$STARTER_SUID" ]; then
            POSTEXTRACT_FIXES="${POSTEXTRACT_FIXES}printf 'abi <abi/4.0>,\ninclude <tunables/global>\n\nprofile peppy-apptainer ${STARTER_SUID} flags=(unconfined) {\n  userns,\n}\n' > /etc/apparmor.d/peppy-apptainer && apparmor_parser -r /etc/apparmor.d/peppy-apptainer && "
        fi

        if [ -n "$POSTEXTRACT_FIXES" ]; then
            apply_sudo_fixes "$POSTEXTRACT_FIXES" "System dependencies configured successfully."
        fi
    fi
    if [ "$PLATFORM" != "apple-darwin" ] && [ -n "${PEPPY_NO_ROOT_INSTALL:-}" ]; then
        echo "Skipped Apptainer setuid setup (PEPPY_NO_ROOT_INSTALL is set)."
        echo "Run 'peppy container setup' later to enable container support."
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

    # Make peppy available for the remainder of this script (and any
    # interactive shell that sources the output, e.g. eval "$(./install.sh)").
    case ":${PATH}:" in
    *":${PEPPY_BIN_DIR}:"*) ;;
    *) export PATH="${PEPPY_BIN_DIR}:${PATH}" ;;
    esac

    render_progress 100 "Installation complete"

    if $PATH_UPDATED; then
        printf "%sSuccessfully added peppy to \$PATH in %s%s\n" "$GREEN" "$PATH_UPDATE_FILE" "$RESET"
    elif $PATH_ALREADY_PRESENT; then
        printf "%speppy is already available in \$PATH via %s%s\n" "$DIM" "$PATH_UPDATE_FILE" "$RESET"
    fi

    print_banner
    echo ""
    printf "%s%s peppy is ready.%s\n\n" "$GREEN" "$OK_MARK" "$RESET"
    if $PATH_UPDATED; then
        echo "To get started, apply the PATH update and run peppy:"
        echo "  source ${PATH_UPDATE_FILE} && peppy"
    else
        echo "To get started:"
        echo "  peppy"
    fi
    echo ""
    echo "For more information visit https://docs.peppy.bot"
} && __wrap__ "$@"
