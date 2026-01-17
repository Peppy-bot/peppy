#!/bin/sh
set -eu

# Build and publish a GitHub Release for peppy for the current host architecture.
#
# Requires:
#   - GITHUB_PEPPY_RELEASE_TOKEN env var (repo-scoped token)
#   - git, curl, python3, cargo, rustc, tar
#
# Outputs:
#   - A tar.gz archive in ./dist/ named like: peppy-x86_64-unknown-linux-gnu.tar.gz

__wrap__() {
    die() {
        echo "error: $*" >&2
        exit 1
    }

    need_cmd() {
        if ! command -v "$1" >/dev/null 2>&1; then
            die "missing required command: $1"
        fi
    }

    prompt() {
        LABEL="$1"
        DEFAULT="${2-}"

        if [ -n "${DEFAULT-}" ]; then
            printf "%s [%s]: " "$LABEL" "$DEFAULT" >&2
        else
            printf "%s: " "$LABEL" >&2
        fi

        IFS= read -r ANSWER || true
        if [ -z "${ANSWER-}" ]; then
            ANSWER="$DEFAULT"
        fi
        printf "%s" "${ANSWER-}"
    }

    prompt_yn() {
        LABEL="$1"
        DEFAULT="${2:-N}" # Y or N

        while :; do
            case "$DEFAULT" in
            Y | y) printf "%s [Y/n]: " "$LABEL" >&2 ;;
            N | n) printf "%s [y/N]: " "$LABEL" >&2 ;;
            *) die "internal error: prompt_yn default must be Y or N" ;;
            esac

            IFS= read -r ANSWER || true
            if [ -z "${ANSWER-}" ]; then
                ANSWER="$DEFAULT"
            fi

            case "$ANSWER" in
            Y | y | yes | YES) return 0 ;;
            N | n | no | NO) return 1 ;;
            *) echo "Please answer y or n." >&2 ;;
            esac
        done
    }

    TEMP_FILES=""
    TEMP_DIRS=""

    mktemp_file() {
        F="$(mktemp "${TMPDIR:-/tmp}/peppy_release.XXXXXXXX")"
        TEMP_FILES="$TEMP_FILES $F"
        printf "%s" "$F"
    }

    mktemp_dir() {
        D="$(mktemp -d "${TMPDIR:-/tmp}/peppy_release_dir.XXXXXXXX")"
        TEMP_DIRS="$TEMP_DIRS $D"
        printf "%s" "$D"
    }

    cleanup() {
        for F in $TEMP_FILES; do
            rm -f "$F"
        done
        for D in $TEMP_DIRS; do
            rm -rf "$D"
        done
    }

    trap cleanup EXIT

    curl_fail_flag() {
        if curl --help all 2>/dev/null | grep -q -- "--fail-with-body"; then
            printf "%s" "--fail-with-body"
        else
            printf "%s" "--fail"
        fi
    }

    print_github_response_headers() {
        HEADER_FILE="$1"
        grep -i -E '^(http/|location:|content-type:|x-github-request-id:|x-ratelimit-remaining:|x-ratelimit-reset:)' "$HEADER_FILE" 2>/dev/null || true
    }

    github_api() {
        METHOD="$1"
        URL="$2"
        DATA_FILE="${3-}"
        BODY_FILE="$(mktemp_file)"
        HEADER_FILE="$(mktemp_file)"
        ERR_FILE="$(mktemp_file)"

        FAIL_FLAG="$(curl_fail_flag)"

        if [ -n "${DATA_FILE-}" ]; then
            curl -sS -L $FAIL_FLAG -X "$METHOD" \
                -H "Authorization: Bearer ${GITHUB_PEPPY_RELEASE_TOKEN}" \
                -H "Accept: application/vnd.github+json" \
                -H "X-GitHub-Api-Version: 2022-11-28" \
                -H "Content-Type: application/json" \
                -D "$HEADER_FILE" \
                --data @"$DATA_FILE" \
                "$URL" >"$BODY_FILE" 2>"$ERR_FILE" || {
                echo "error: GitHub API request failed: $METHOD $URL" >&2
                cat "$ERR_FILE" >&2 || true
                cat "$BODY_FILE" >&2 || true
                exit 1
            }
        else
            curl -sS -L $FAIL_FLAG -X "$METHOD" \
                -H "Authorization: Bearer ${GITHUB_PEPPY_RELEASE_TOKEN}" \
                -H "Accept: application/vnd.github+json" \
                -H "X-GitHub-Api-Version: 2022-11-28" \
                -D "$HEADER_FILE" \
                "$URL" >"$BODY_FILE" 2>"$ERR_FILE" || {
                echo "error: GitHub API request failed: $METHOD $URL" >&2
                cat "$ERR_FILE" >&2 || true
                cat "$BODY_FILE" >&2 || true
                exit 1
            }
        fi

        HTTP_CODE="$(
            python3 - "$HEADER_FILE" 2>/dev/null <<'PY' || true
import sys

code = ""
with open(sys.argv[1], "r", encoding="utf-8", errors="replace") as f:
    for line in f:
        if line.startswith("HTTP/"):
            parts = line.split()
            if len(parts) >= 2:
                code = parts[1]
print(code)
PY
        )"

        if [ ! -s "$BODY_FILE" ]; then
            case "${HTTP_CODE-}" in
            204 | 205) return 0 ;;
            esac
            echo "error: GitHub API returned an empty response (${HTTP_CODE:-unknown}) for $METHOD $URL" >&2
            echo "Response headers:" >&2
            print_github_response_headers "$HEADER_FILE" >&2
            cat "$ERR_FILE" >&2 || true
            exit 1
        fi

        JSON_ERR_FILE="$(mktemp_file)"
        if ! python3 - "$BODY_FILE" >/dev/null 2>"$JSON_ERR_FILE" <<'PY'; then
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    json.load(f)
PY
            echo "error: GitHub API returned a non-JSON response (${HTTP_CODE:-unknown}) for $METHOD $URL" >&2
            echo "Response headers:" >&2
            print_github_response_headers "$HEADER_FILE" >&2
            echo "Response body (first 2000 bytes):" >&2
            head -c 2000 "$BODY_FILE" >&2 || true
            cat "$JSON_ERR_FILE" >&2 || true
            exit 1
        fi

        cat "$BODY_FILE"
    }

    github_upload_asset() {
        RELEASE_ID="$1"
        ASSET_NAME="$2"
        ASSET_PATH="$3"
        OWNER="$4"
        REPO="$5"

        BODY_FILE="$(mktemp_file)"
        ERR_FILE="$(mktemp_file)"

        FAIL_FLAG="$(curl_fail_flag)"
        UPLOAD_URL="https://uploads.github.com/repos/${OWNER}/${REPO}/releases/${RELEASE_ID}/assets?name=${ASSET_NAME}"

        curl -sS -L $FAIL_FLAG -X POST \
            -H "Authorization: Bearer ${GITHUB_PEPPY_RELEASE_TOKEN}" \
            -H "Accept: application/vnd.github+json" \
            -H "X-GitHub-Api-Version: 2022-11-28" \
            -H "Content-Type: application/octet-stream" \
            --data-binary @"$ASSET_PATH" \
            "$UPLOAD_URL" >"$BODY_FILE" 2>"$ERR_FILE" || {
            echo "error: failed to upload asset '${ASSET_NAME}'" >&2
            cat "$ERR_FILE" >&2 || true
            cat "$BODY_FILE" >&2 || true
            exit 1
        }

        cat "$BODY_FILE"
    }

    github_repo_slug() {
        if [ -n "${GITHUB_REPOSITORY:-}" ]; then
            printf "%s" "$GITHUB_REPOSITORY"
            return 0
        fi

        REMOTE_URL="$(git config --get remote.origin.url 2>/dev/null || true)"
        if [ -z "${REMOTE_URL-}" ]; then
            die "could not determine repo (set GITHUB_REPOSITORY=owner/repo or configure git remote 'origin')"
        fi

        case "$REMOTE_URL" in
        git@github.com:*)
            SLUG="${REMOTE_URL#git@github.com:}"
            ;;
        https://github.com/*)
            SLUG="${REMOTE_URL#https://github.com/}"
            ;;
        http://github.com/*)
            SLUG="${REMOTE_URL#http://github.com/}"
            ;;
        ssh://git@github.com/*)
            SLUG="${REMOTE_URL#ssh://git@github.com/}"
            ;;
        *)
            die "unsupported remote url (expected github.com): $REMOTE_URL"
            ;;
        esac

        SLUG="${SLUG%.git}"
        printf "%s" "$SLUG"
    }

    parse_release_response() {
        JSON_FILE="$1"
        python3 - "$JSON_FILE" <<'PY'
import json
import sys

path = sys.argv[1]
try:
    with open(path, "r", encoding="utf-8") as f:
        obj = json.load(f)
except Exception as e:
    print(f"error: failed to parse GitHub API JSON response: {e}", file=sys.stderr)
    print("Response body (first 2000 bytes):", file=sys.stderr)
    try:
        with open(path, "rb") as f:
            data = f.read(2000)
        sys.stderr.write(data.decode("utf-8", errors="replace"))
        sys.stderr.write("\n")
    except Exception:
        pass
    sys.exit(1)

if not isinstance(obj, dict):
    print("error: unexpected GitHub API response (expected JSON object)", file=sys.stderr)
    print(json.dumps(obj, indent=2)[:2000], file=sys.stderr)
    sys.exit(1)

rid = obj.get("id")
url = obj.get("html_url", "")
if rid is None:
    msg = obj.get("message") or "missing 'id' in response"
    print(f"error: unexpected GitHub API response: {msg}", file=sys.stderr)
    print(json.dumps(obj, indent=2)[:2000], file=sys.stderr)
    sys.exit(1)

print(rid)
print(url)
PY
    }

    delete_asset_if_exists() {
        RELEASE_ID="$1"
        ASSET_NAME="$2"
        OWNER="$3"
        REPO="$4"

        ASSETS_JSON="$(github_api GET "https://api.github.com/repos/${OWNER}/${REPO}/releases/${RELEASE_ID}/assets")"
        ASSET_ID="$(
            printf "%s" "$ASSETS_JSON" | ASSET_NAME="$ASSET_NAME" python3 - <<'PY'
import json
import os
import sys

name = os.environ["ASSET_NAME"]
raw = sys.stdin.read()
if not raw.strip():
    # Some environments/proxies can yield an empty body here; treat it as "no assets".
    sys.exit(0)
try:
    assets = json.loads(raw)
except Exception as e:
    print(f"error: failed to parse GitHub assets JSON: {e}", file=sys.stderr)
    print("Response body (first 2000 bytes):", file=sys.stderr)
    print(raw[:2000], file=sys.stderr)
    sys.exit(1)
for a in assets:
    if a.get("name") == name:
        print(a.get("id", ""))
        break
PY
        )"

        if [ -n "${ASSET_ID-}" ]; then
            github_api DELETE "https://api.github.com/repos/${OWNER}/${REPO}/releases/assets/${ASSET_ID}" >/dev/null
        fi
    }

    if [ -z "${GITHUB_PEPPY_RELEASE_TOKEN:-}" ]; then
        die "GITHUB_PEPPY_RELEASE_TOKEN env var is required"
    fi

    need_cmd git
    need_cmd curl
    need_cmd python3
    need_cmd cargo
    need_cmd rustc
    need_cmd tar

    REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || die "must be run inside a git repository"
    cd "$REPO_ROOT"

    TARGET_COMMITISH="${PEPPY_RELEASE_TARGET:-main}"

    CURRENT_BRANCH="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
    if [ -n "${CURRENT_BRANCH-}" ] && [ "${CURRENT_BRANCH-}" != "$TARGET_COMMITISH" ]; then
        if ! prompt_yn "Current branch is '${CURRENT_BRANCH}'. Release will target '${TARGET_COMMITISH}'. Continue?" N; then
            exit 1
        fi
    fi

    if ! git diff --quiet >/dev/null 2>&1 || ! git diff --cached --quiet >/dev/null 2>&1; then
        if ! prompt_yn "Working tree has uncommitted changes. Continue?" N; then
            exit 1
        fi
    fi

    TAG="$(prompt "Tag of the release (example: v0.0.1)")"
    [ -n "${TAG-}" ] || die "release tag cannot be empty"
    TITLE="$(prompt "Release title")"
    [ -n "${TITLE-}" ] || die "release title cannot be empty"

    GENERATE_NOTES=false
    NOTES_FILE=""
    if prompt_yn "Generate release notes automatically?" Y; then
        GENERATE_NOTES=true
    else
        NOTES_FILE="$(mktemp_file)"
        {
            echo "# Write release notes below. Lines starting with # will be ignored."
            echo "# Save and close the editor when you're done."
            echo ""
        } >"$NOTES_FILE"

        EDITOR_CMD="${EDITOR:-}"
        if [ -z "${EDITOR_CMD-}" ]; then
            if command -v nano >/dev/null 2>&1; then
                EDITOR_CMD="nano"
            else
                EDITOR_CMD="vi"
            fi
        fi

        "$EDITOR_CMD" "$NOTES_FILE"

        CLEAN_NOTES_FILE="$(mktemp_file)"
        python3 - <<'PY' "$NOTES_FILE" "$CLEAN_NOTES_FILE"
import sys

src, dst = sys.argv[1], sys.argv[2]
with open(src, "r", encoding="utf-8") as f:
    lines = [ln for ln in f.readlines() if not ln.startswith("#")]
with open(dst, "w", encoding="utf-8") as f:
    f.write("".join(lines).strip() + "\n")
PY
        NOTES_FILE="$CLEAN_NOTES_FILE"
    fi

    HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
    [ -n "${HOST_TRIPLE-}" ] || die "could not determine Rust host target triple"
    case "$HOST_TRIPLE" in
    aarch64-apple-darwin | x86_64-unknown-linux-gnu | x86_64-unknown-linux-musl | aarch64-unknown-linux-gnu | aarch64-unknown-linux-musl | armv7-unknown-linux-gnueabihf | arm-unknown-linux-gnueabihf) ;;
    *)
        die "unsupported host target '$HOST_TRIPLE' (supported: aarch64-apple-darwin, x86_64-unknown-linux-gnu, x86_64-unknown-linux-musl, aarch64-unknown-linux-gnu, aarch64-unknown-linux-musl, armv7-unknown-linux-gnueabihf)"
        ;;
    esac

    ASSET_NAME="peppy-${HOST_TRIPLE}.tar.gz"
    DIST_DIR="${PEPPY_DIST_DIR:-$REPO_ROOT/dist}"
    mkdir -p "$DIST_DIR"
    ASSET_PATH="${DIST_DIR%/}/${ASSET_NAME}"

    echo "Building peppy for ${HOST_TRIPLE}..."
    cargo build -p peppy --bin peppy --release --locked --target "$HOST_TRIPLE"

    TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
    BIN_PATH="${TARGET_DIR%/}/${HOST_TRIPLE}/release/peppy"
    if [ ! -f "$BIN_PATH" ]; then
        BIN_PATH="${TARGET_DIR%/}/release/peppy"
    fi
    [ -f "$BIN_PATH" ] || die "peppy binary not found (expected '${TARGET_DIR%/}/${HOST_TRIPLE}/release/peppy')"

    PKG_DIR="$(mktemp_dir)"
    cp "$BIN_PATH" "$PKG_DIR/peppy"
    chmod +x "$PKG_DIR/peppy"

    # `peppy service serve` spawns `zenohd`; include the built zenohd binary next to peppy.
    ZENOHD_PATH="$(find "${TARGET_DIR%/}/${HOST_TRIPLE}/release/build" -type f -path "*/pmi-*/out/zenohd" -print | head -n 1 || true)"
    [ -f "${ZENOHD_PATH-}" ] || die "zenohd binary not found in target dir (expected it under '${TARGET_DIR%/}/${HOST_TRIPLE}/release/build/pmi-*/out/zenohd')"
    cp "$ZENOHD_PATH" "$PKG_DIR/zenohd"
    chmod +x "$PKG_DIR/zenohd"

    tar -czf "$ASSET_PATH" -C "$PKG_DIR" .
    echo "Built artifact: $ASSET_PATH"

    SLUG="$(github_repo_slug)"
    OWNER="${SLUG%%/*}"
    REPO="${SLUG#*/}"
    if [ -z "${OWNER-}" ] || [ -z "${REPO-}" ] || [ "$OWNER" = "$REPO" ]; then
        die "invalid GITHUB_REPOSITORY/repo slug: $SLUG"
    fi

    PAYLOAD_FILE="$(mktemp_file)"
    TAG="$TAG" TITLE="$TITLE" TARGET_COMMITISH="$TARGET_COMMITISH" GENERATE_NOTES="$GENERATE_NOTES" NOTES_FILE="$NOTES_FILE" \
        python3 - <<'PY' >"$PAYLOAD_FILE"
import json
import os

tag = os.environ["TAG"]
title = os.environ["TITLE"]
target = os.environ.get("TARGET_COMMITISH", "main")
generate = os.environ.get("GENERATE_NOTES", "false").lower() in ("1", "true", "yes", "y")
notes_file = os.environ.get("NOTES_FILE", "")

data = {
    "tag_name": tag,
    "name": title,
    "target_commitish": target,
}

if generate:
    data["generate_release_notes"] = True
else:
    body = ""
    if notes_file:
        try:
            with open(notes_file, "r", encoding="utf-8") as f:
                body = f.read()
        except FileNotFoundError:
            body = ""
    data["body"] = body

print(json.dumps(data))
PY

    echo "Creating GitHub release ${SLUG}@${TAG}..."
    RELEASE_JSON_FILE="$(mktemp_file)"
    github_api POST "https://api.github.com/repos/${OWNER}/${REPO}/releases" "$PAYLOAD_FILE" >"$RELEASE_JSON_FILE"

    RELEASE_INFO="$(parse_release_response "$RELEASE_JSON_FILE")" || exit 1
    RELEASE_ID="$(printf "%s\n" "$RELEASE_INFO" | sed -n '1p')"
    RELEASE_URL="$(printf "%s\n" "$RELEASE_INFO" | sed -n '2p')"

    delete_asset_if_exists "$RELEASE_ID" "$ASSET_NAME" "$OWNER" "$REPO"
    echo "Uploading ${ASSET_NAME}..."
    github_upload_asset "$RELEASE_ID" "$ASSET_NAME" "$ASSET_PATH" "$OWNER" "$REPO" >/dev/null

    echo "Release created: ${RELEASE_URL:-https://github.com/${SLUG}/releases/tag/${TAG}}"
} && __wrap__
