#!/usr/bin/env bash
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TMP_DIR="$ROOT_DIR/tmp"

usage() {
  cat <<'EOF'
Usage: scripts/test-against-repo.sh [repo] [ctxlint-args...]

repo: mattpocock | google | microsoft | <owner/repo> | <git-url>
      (omit to pick interactively; defaults to mattpocock when non-interactive)

Clones (or updates) the target repo into ./tmp/, builds ctxlint, and
runs it against the clone. Extra args are passed through to ctxlint.

Examples:
  scripts/test-against-repo.sh
  scripts/test-against-repo.sh google --strict
  scripts/test-against-repo.sh microsoft --format json
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

pick_repo() {
  local options=("mattpocock" "google" "microsoft" "custom (owner/repo or git URL)")
  local n=${#options[@]}
  local selected=0
  local key esc

  if [[ ! -t 0 ]]; then
    echo "mattpocock"
    return
  fi

  draw() {
    local i
    for i in "${!options[@]}"; do
      printf '\033[K' >&2
      if [[ $i -eq $selected ]]; then
        printf '  > %s\n' "${options[$i]}" >&2
      else
        printf '    %s\n' "${options[$i]}" >&2
      fi
    done
  }

  echo "Select a repo to test against (up/down + enter):" >&2
  draw
  tput civis >&2 2>/dev/null

  while true; do
    IFS= read -rsn1 key
    if [[ $key == $'\x1b' ]]; then
      local k2 k3
      k2="" k3=""
      IFS= read -rsn1 -t 1 k2
      if [[ $k2 == "[" ]]; then
        IFS= read -rsn1 -t 1 k3
      fi
      key="$key$k2$k3"
    fi

    case "$key" in
      $'\x1b[A'|k) # up
        selected=$(((selected - 1 + n) % n))
        ;;
      $'\x1b[B'|j) # down
        selected=$(((selected + 1) % n))
        ;;
      "") # enter
        break
        ;;
    esac

    tput cuu "$n" >&2 2>/dev/null
    draw
  done

  tput cnorm >&2 2>/dev/null

  if [[ $selected -eq $((n - 1)) ]]; then
    local custom
    read -rp "Enter repo (owner/repo or git URL): " custom
    echo "$custom"
    return
  fi

  echo "${options[$selected]}"
}

if [[ $# -gt 0 && "$1" != -* ]]; then
  REPO_ARG="$1"
  shift
else
  REPO_ARG="$(pick_repo)"
fi

case "$REPO_ARG" in
  mattpocock) CLONE_URL="https://github.com/mattpocock/skills" ;;
  google)     CLONE_URL="https://github.com/google/skills" ;;
  microsoft)  CLONE_URL="https://github.com/microsoft/skills" ;;
  *)          CLONE_URL="" ;;
esac

if [[ -n "$CLONE_URL" ]]; then
  REPO_NAME="${REPO_ARG}-skills"
elif [[ "$REPO_ARG" == http*://* || "$REPO_ARG" == git@* ]]; then
  CLONE_URL="$REPO_ARG"
  REPO_NAME="$(basename "$REPO_ARG" .git)"
elif [[ "$REPO_ARG" == */* ]]; then
  CLONE_URL="https://github.com/$REPO_ARG"
  REPO_NAME="${REPO_ARG//\//-}"
else
  echo "error: unknown repo '$REPO_ARG'" >&2
  usage
  exit 2
fi

CLONE_DIR="$TMP_DIR/$REPO_NAME"
mkdir -p "$TMP_DIR"

if [[ -d "$CLONE_DIR/.git" ]]; then
  echo "==> Updating existing clone at $CLONE_DIR"
  git -C "$CLONE_DIR" pull --ff-only || echo "warning: pull failed, using existing clone as-is"
else
  echo "==> Cloning $CLONE_URL into $CLONE_DIR"
  git clone --depth 1 "$CLONE_URL" "$CLONE_DIR"
fi

echo "==> Building ctxlint"
(cd "$ROOT_DIR" && cargo build --release)

echo "==> Running ctxlint against $CLONE_DIR"
# The clone lives under ctxlint's own repository, so config discovery would
# find ctxlint's .ctxlint.yaml and quietly apply its settings to somebody
# else's repo. Ignore it, unless the caller asked for a config themselves.
CONFIG_ARGS=(--no-config)
for arg in "$@"; do
  case "$arg" in
    --config | --config=* | --no-config) CONFIG_ARGS=() ;;
  esac
done
"$ROOT_DIR/target/release/ctxlint" ${CONFIG_ARGS[@]+"${CONFIG_ARGS[@]}"} "$@" "$CLONE_DIR"
STATUS=$?

echo "==> ctxlint exited with status $STATUS"
exit "$STATUS"
