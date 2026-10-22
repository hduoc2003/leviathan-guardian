#!/usr/bin/env bash
set -euo pipefail

# Resolves a published guardian image to its content digest, checks that the
# digest carries a GitHub build attestation, pins it into the Phala compose and
# deploys the result.
#
# Usage:
#   ./scripts/deploy-phala.sh <registry/repo:tag> --repo <owner/name> \
#     [--cvm-id <id>] [--env-file <path>] [--reset-db] [--dry-run]
#
# The image is built by .github/workflows/docker-publish.yml, never here.
#
# The digest is written into deploy/phala/docker-compose.yaml rather than passed
# as a variable: dstack hashes that file into `compose_hash`, and the hash is
# what gets approved on chain. A tag would let a different image run under an
# already-approved hash.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE="$REPO_ROOT/deploy/phala/docker-compose.yaml"

IMAGE=""
GH_REPO=""
CVM_ID=""
ENV_FILE="${GUARDIAN_PHALA_ENV_FILE:-$HOME/code/softly/leviathan-vps-secrets/guardian-phala.env}"
DRY_RUN=0
RESET_DB=0
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) GH_REPO="$2"; shift 2 ;;
    --cvm-id) CVM_ID="$2"; shift 2 ;;
    --env-file) ENV_FILE="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --reset-db) RESET_DB=1; shift ;;
    -*) echo "unknown flag: $1" >&2; exit 2 ;;
    *) IMAGE="$1"; shift ;;
  esac
done
[ -n "$IMAGE" ] && [ -n "$GH_REPO" ] || { echo "usage: $0 <registry/repo:tag> --repo <owner/name> [--cvm-id <id>] [--env-file <path>] [--reset-db] [--dry-run]" >&2; exit 2; }
[ "$RESET_DB" -eq 0 ] || [ "$DRY_RUN" -eq 0 ] || { echo "--reset-db needs a deploy; drop --dry-run" >&2; exit 2; }
case "${IMAGE##*/}" in
  *:*) ;;
  *) echo "expected <registry/repo:tag>, got '$IMAGE'" >&2; exit 2 ;;
esac
REPO_REF="${IMAGE%:*}"

# Hashing the raw manifest is the definition of the digest, and unlike
# `--format` it does not depend on the buildx version.
echo ">> Resolving $IMAGE"
SHA="sha256:$(docker buildx imagetools inspect --raw "$IMAGE" | sha256sum | cut -d' ' -f1)"
[ "$SHA" != "sha256:" ] || { echo "could not resolve $IMAGE; has the publish workflow run?" >&2; exit 1; }
DIGEST="$REPO_REF@$SHA"
echo ">> Digest $DIGEST"

# The attestation is what links these bytes to a commit. Without it the compose
# hash pins bytes of unknown origin, which is the thing the on-chain approval is
# supposed to rule out.
echo ">> Verifying build provenance against $GH_REPO"
gh attestation verify "oci://$DIGEST" --repo "$GH_REPO"

python3 - "$COMPOSE" "$REPO_REF" "$DIGEST" <<'PY'
import re, sys
path, repo_ref, digest = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(path).read()
# Matches the guardian line only - either the committed placeholder or an
# already-pinned digest for this repository - so the sidecar images keep theirs.
pattern = (
    r'^(\s*image:\s*)(?:'
    + re.escape(repo_ref)
    + r'[@:]|REGISTRY/guardian@sha256:0{64}).*$'
)
new, count = re.subn(pattern, lambda m: m.group(1) + digest, text, flags=re.M)
if count != 1:
    sys.exit(f"expected exactly one {repo_ref} image line in {path}, replaced {count}")
open(path, 'w').write(new)
print(f">> Pinned {path}")
PY

# A placeholder digest reaching a deploy would mean the approved hash pins
# nothing, so refuse rather than ship it.
grep -q "sha256:0\{64\}" "$COMPOSE" && { echo "compose still holds the placeholder digest" >&2; exit 1; }

if [ "$DRY_RUN" -eq 1 ]; then
  echo ">> Dry run; not deploying. Compose is pinned and ready:"
  grep "image:" "$COMPOSE"
  exit 0
fi

[ -n "$CVM_ID" ] || { echo "--cvm-id is required to deploy (omit it with --dry-run)" >&2; exit 2; }

# Postgres applies the password only at initdb; a new value would lock guardian out.
[ -r "$ENV_FILE" ] || { echo "env file $ENV_FILE is missing or unreadable" >&2; exit 1; }
grep -qE '^POSTGRES_PASSWORD=[A-Za-z0-9]+$' "$ENV_FILE" \
  || { echo "$ENV_FILE has no URL-safe POSTGRES_PASSWORD=<alphanumeric> line" >&2; exit 1; }

# Probed before deploying so a CVM without a shell fails before anything changes.
if [ "$RESET_DB" -eq 1 ]; then
  phala ssh "$CVM_ID" -- true \
    || { echo "--reset-db needs phala ssh into CVM $CVM_ID, which only the dev OS image allows (not the Production OS image)" >&2; exit 1; }
fi

echo ">> Deploying to CVM $CVM_ID"
phala deploy --cvm-id "$CVM_ID" --compose "$COMPOSE" -e "$ENV_FILE" --wait

# Keeps the keystore volume (ACK keys), migrations and the dstack-bound storage_encryption_marker.
if [ "$RESET_DB" -eq 1 ]; then
  echo ">> Resetting guardian chain state on CVM $CVM_ID"
  phala ssh "$CVM_ID" -- '
    set -e
    G=$(docker ps -q --filter label=com.docker.compose.service=guardian)
    P=$(docker ps -q --filter label=com.docker.compose.service=postgres)
    [ -n "$G" ] && [ -n "$P" ] || { echo "guardian or postgres container not running" >&2; exit 1; }
    healthy() { for i in $(seq 1 30); do wget -qO- http://127.0.0.1:3000/status 2>/dev/null | grep -q "\"status\":\"ok\"" && return 0; sleep 2; done; return 1; }
    healthy || { echo "guardian not healthy before the reset" >&2; exit 1; }
    before=$(wget -qO- http://127.0.0.1:3000/pubkey)
    trap "docker start $G >/dev/null" EXIT
    docker stop "$G" >/dev/null
    docker exec "$P" psql -U guardian -d guardian -v ON_ERROR_STOP=1 -c "BEGIN; TRUNCATE account_auth_state, account_metadata, admin_actions, auth_challenges, auth_sessions, dashboard_stats_snapshots, delta_proposals, deltas, states, worker_leases RESTART IDENTITY CASCADE; COMMIT;"
    docker start "$G" >/dev/null
    healthy || { echo "guardian not healthy after the reset" >&2; exit 1; }
    after=$(wget -qO- http://127.0.0.1:3000/pubkey)
    [ "$before" = "$after" ] || { echo "ACK key changed across the reset: $before -> $after" >&2; exit 1; }
    echo "reset done; ACK key unchanged: $after"
  ' || { echo "chain-state reset FAILED on CVM $CVM_ID" >&2; exit 1; }
fi

echo
echo ">> Deployed. If this app uses Onchain KMS, the new compose_hash must be"
echo ">> registered with addComposeHash before the CVM can obtain its keys."
