#!/bin/sh
set -eu

# Install peppy from GitHub Releases.
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Peppy-bot/peppy/main/scripts/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/Peppy-bot/peppy/main/scripts/install.sh | sh -s -- ./peppy-x86_64-unknown-linux-gnu.tar.gz
#   ./scripts/install.sh ./peppy-x86_64-unknown-linux-gnu.tar.gz
#
# Environment variables:
#   PEPPY_VERSION           Version to install (default: latest)
#   PEPPY_HOME              Install prefix (default: ~/.peppy)
#   PEPPY_BIN_DIR           Binary install directory (default: $PEPPY_HOME/bin)
#   PEPPY_PLATFORM          Linux target suffix (default: auto-detected)
#   PEPPY_ARCH              Override detected architecture (e.g. aarch64, x86_64)
#   PEPPY_REPOURL           GitHub repo URL (default: https://github.com/Peppy-bot/peppy)
#   PEPPY_DOWNLOAD_URL      Override full download URL
#   PEPPY_NO_PATH_UPDATE    If set, do not update shell PATH config

__wrap__() {
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
            echo "  ./scripts/install.sh [path/to/peppy-<arch>-<platform>.tar.gz]"
            echo "  curl -fsSL https://raw.githubusercontent.com/Peppy-bot/peppy/main/scripts/install.sh | sh -s -- [path/to/archive.tar.gz]"
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

    REPOURL="${PEPPY_REPOURL:-https://github.com/Peppy-bot/peppy}"
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

    if ! command -v tar >/dev/null 2>&1; then
        echo "error: 'tar' is required to install peppy" >&2
        exit 1
    fi

    BINARY="peppy-${ARCH}-${PLATFORM}"
    EXTENSION=".tar.gz"

    if [ "$VERSION" = "latest" ]; then
        DOWNLOAD_URL="${PEPPY_DOWNLOAD_URL:-${REPOURL%/}/releases/latest/download/${BINARY}${EXTENSION-}}"
    else
        DOWNLOAD_URL="${PEPPY_DOWNLOAD_URL:-${REPOURL%/}/releases/download/v${VERSION#v}/${BINARY}${EXTENSION-}}"
    fi

    if [ -n "${ARCHIVE_PATH-}" ]; then
        printf "This script will install peppy for you.\nUsing local archive: %s\n" "$ARCHIVE_PATH"
    else
        printf "This script will automatically download and install peppy (%s) for you.\nGetting it from this url: %s\n" "$VERSION" "$(mask_credentials "$DOWNLOAD_URL")"
    fi

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
        cp "$ARCHIVE_PATH" "$TEMP_FILE"
    else
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

        if [ ! -t 1 ]; then
            CURL_OPTIONS="-sS"
            WGET_OPTIONS="--no-verbose"
        else
            CURL_OPTIONS=""
            WGET_OPTIONS="--show-progress"
        fi

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
    fi

    if [ ! -s "$TEMP_FILE" ]; then
        echo "error: temporary file ${TEMP_FILE} not correctly created." >&2
        echo "       As a workaround, you can try setting TMPDIR env variable to a directory with write permissions." >&2
        exit 1
    fi

    mkdir -p "$PEPPY_BIN_DIR"
    TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/.peppy_install_dir.XXXXXXXX")"
    tar -xzf "$TEMP_FILE" -C "$TEMP_DIR"

    PEPPY_PATH=""
    if [ -f "$TEMP_DIR/peppy" ]; then
        PEPPY_PATH="$TEMP_DIR/peppy"
    else
        PEPPY_PATH="$(find "$TEMP_DIR" -type f -name peppy -print | head -n 1 || true)"
    fi

    if [ -z "${PEPPY_PATH-}" ] || [ ! -f "$PEPPY_PATH" ]; then
        echo "error: could not find the 'peppy' binary in the downloaded archive." >&2
        exit 1
    fi

    ZENOHD_PATH=""
    if [ -f "$TEMP_DIR/zenohd" ]; then
        ZENOHD_PATH="$TEMP_DIR/zenohd"
    else
        ZENOHD_PATH="$(find "$TEMP_DIR" -type f -name zenohd -print | head -n 1 || true)"
    fi

    mv "$PEPPY_PATH" "$PEPPY_BIN_DIR/peppy"
    chmod +x "$PEPPY_BIN_DIR/peppy"

    if [ -n "${ZENOHD_PATH-}" ] && [ -f "$ZENOHD_PATH" ]; then
        mv "$ZENOHD_PATH" "$PEPPY_BIN_DIR/zenohd"
        chmod +x "$PEPPY_BIN_DIR/zenohd"
    fi

    if [ "$PEPPY_BIN_DIR" = "$PEPPY_HOME/bin" ]; then
        echo "The 'peppy' binary is installed into '${PEPPY_HOME}'"
    else
        echo "The 'peppy' binary is installed into '${PEPPY_BIN_DIR}'"
    fi
    if [ ! -f "$PEPPY_BIN_DIR/zenohd" ]; then
        echo "warning: 'zenohd' was not found in the archive. 'peppy service serve' requires zenohd on PATH or next to the peppy binary." >&2
    fi

    if [ -z "${PEPPY_NO_SERVICE_INSTALL:-}" ]; then
        SERVICE_INSTALL_OUTPUT=""
        if ! SERVICE_INSTALL_OUTPUT=$("$PEPPY_BIN_DIR/peppy" service install 2>&1); then
            case "$SERVICE_INSTALL_OUTPUT" in
            *"Permission denied"* | *"permission denied"* | *"os error 13"*)
                echo "warning: failed to install the peppy background service (permission denied; try: 'sudo $PEPPY_BIN_DIR/peppy service install')" >&2
                ;;
            *)
                echo "warning: failed to install the peppy background service (try: '$PEPPY_BIN_DIR/peppy service install')" >&2
                ;;
            esac

            if [ -n "${PEPPY_DEBUG:-}" ] && [ -n "${SERVICE_INSTALL_OUTPUT-}" ]; then
                echo "$SERVICE_INSTALL_OUTPUT" >&2
            fi
        fi
    else
        echo "No service install because PEPPY_NO_SERVICE_INSTALL is set"
    fi

    if [ -n "${PEPPY_NO_PATH_UPDATE:-}" ]; then
        echo "No path update because PEPPY_NO_PATH_UPDATE is set"
    else
        update_shell() {
            FILE="$1"
            LINE="$2"

            if [ ! -f "$FILE" ]; then
                touch "$FILE"
            fi

            if ! grep -Fxq "$LINE" "$FILE"; then
                echo "Updating '${FILE}'"
                echo >>"$FILE"
                echo "$LINE" >>"$FILE"
                echo "Please restart your shell or source your shell config."
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
            echo "warn: Could not detect shell type." >&2
            echo "      Please permanently add '${PEPPY_BIN_DIR}' to your \$PATH to enable the 'peppy' command." >&2
            ;;

        *)
            echo "warn: Could not update shell $(basename "$SHELL")" >&2
            echo "      Please permanently add '${PEPPY_BIN_DIR}' to your \$PATH to enable the 'peppy' command." >&2
            ;;
        esac
    fi
} && __wrap__ "$@"
