#!/bin/bash
set -euo pipefail

# Publishes the TypeScript packages to the Leviathan Gitea npm registry,
# stamping the commit they were built from into the version - the same scheme
# the fork's WASM SDK uses (e.g. 0.15.0-node.5e72c326).
#
# Usage:
#   GITEA_NPM_TOKEN=... ./scripts/publish-npm.sh [package-dir ...] [--dry-run]
#
# With no package arguments it publishes all four, in dependency order.
# Override the prerelease id with VERSION_TAG (defaults to `node`).
#
# Consumers must map the scope to this registry, otherwise npm looks for these
# versions on public npm and fails:
#   @openzeppelin:registry=https://git.softly.com/api/packages/leviathan/npm/

REGISTRY="https://git.softly.com/api/packages/leviathan/npm/"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# guardian-client first: miden-multisig-client depends on it.
DEFAULT_PACKAGES=(
  guardian-client
  guardian-operator-client
  guardian-evm-client
  miden-multisig-client
)

PACKAGES=()
DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -*) echo "unknown flag: $arg" >&2; exit 2 ;;
    *) PACKAGES+=("$arg") ;;
  esac
done
[ ${#PACKAGES[@]} -eq 0 ] && PACKAGES=("${DEFAULT_PACKAGES[@]}")

[ -n "${GITEA_NPM_TOKEN:-}" ] || { echo "GITEA_NPM_TOKEN is not set" >&2; exit 1; }

# The version claims to identify a commit, so refuse to publish a tree that does
# not match one. Without this the sha in the version is a lie.
if [ -n "$(git -C "$REPO_ROOT" status --porcelain)" ]; then
  echo "working tree is dirty; commit or stash before publishing" >&2
  git -C "$REPO_ROOT" status --short >&2
  exit 1
fi

SHA="$(git -C "$REPO_ROOT" rev-parse --short=8 HEAD)"

for package in "${PACKAGES[@]}"; do
  [ -f "$REPO_ROOT/packages/$package/package.json" ] \
    || { echo "no package at packages/$package" >&2; exit 1; }
done

# Auth goes in a throwaway config rather than the repo's .npmrc, so no token is
# ever written into a tracked file and packages without their own .npmrc work.
NPM_CONFIG="$(mktemp)"
BACKUP_DIR="$(mktemp -d)"
cleanup() {
  for package in "${PACKAGES[@]}"; do
    [ -f "$BACKUP_DIR/$package.json" ] \
      && cp "$BACKUP_DIR/$package.json" "$REPO_ROOT/packages/$package/package.json"
  done
  rm -rf "$NPM_CONFIG" "$BACKUP_DIR"
}
trap cleanup EXIT

printf '//git.softly.com/api/packages/leviathan/npm/:_authToken=%s\n' "$GITEA_NPM_TOKEN" > "$NPM_CONFIG"
export npm_config_userconfig="$NPM_CONFIG"

for package in "${PACKAGES[@]}"; do
  package_dir="$REPO_ROOT/packages/$package"
  cp "$package_dir/package.json" "$BACKUP_DIR/$package.json"

  # Stamp the version and repoint every in-repo dependency at the exact same
  # build. A range like ^0.16.1 would resolve against public npm and silently
  # hand the consumer the upstream package instead of this fork.
  VERSION="$(SHA="$SHA" VERSION_TAG="${VERSION_TAG:-node}" node -e '
    const fs = require("fs");
    const file = process.argv[1];
    const own = new Set(process.argv.slice(2));
    const pkg = JSON.parse(fs.readFileSync(file, "utf8"));
    const version = `${pkg.version.split("-")[0]}-${process.env.VERSION_TAG}.${process.env.SHA}`;
    pkg.version = version;
    for (const field of ["dependencies", "peerDependencies"]) {
      for (const name of Object.keys(pkg[field] ?? {})) {
        if (own.has(name)) pkg[field][name] = version;
      }
    }
    fs.writeFileSync(file, JSON.stringify(pkg, null, 2) + "\n");
    console.log(version);
  ' "$package_dir/package.json" \
    @openzeppelin/guardian-client \
    @openzeppelin/guardian-operator-client \
    @openzeppelin/guardian-evm-client \
    @openzeppelin/miden-multisig-client)"

  echo "=== $package -> $VERSION ==="
  (cd "$package_dir" && npm run build && npm test)

  if [ "$DRY_RUN" -eq 1 ]; then
    (cd "$package_dir" && npm publish --registry="$REGISTRY" --dry-run)
  else
    (cd "$package_dir" && npm publish --registry="$REGISTRY")
    echo "published $package@$VERSION"
  fi
done

[ "$DRY_RUN" -eq 1 ] && echo "dry run only; nothing was published"

# Every package.json is restored by the trap: the stamped version belongs to the
# published artifact, not to the working tree.
