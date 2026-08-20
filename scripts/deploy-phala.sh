#!/usr/bin/env bash
set -euo pipefail

# Resolves a published guardian image to its content digest, checks that the
# digest carries a GitHub build attestation, pins it into the Phala compose and
# deploys the result.
#
# Usage:
#   ./scripts/deploy-phala.sh <registry/repo:tag> [--repo <owner/name>] \
#     [--cvm-id <id>] [--dry-run]
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
DRY_RUN=0
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) GH_REPO="$2"; shift 2 ;;
    --cvm-id) CVM_ID="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -*) echo "unknown flag: $1" >&2; exit 2 ;;
    *) IMAGE="$1"; shift ;;
  esac
done
[ -n "$IMAGE" ] || { echo "usage: $0 <registry/repo:tag> [--repo <owner/name>] [--cvm-id <id>] [--dry-run]" >&2; exit 2; }
case "${IMAGE##*/}" in
  *:*) ;;
  *) echo "expected <registry/repo:tag>, got '$IMAGE'" >&2; exit 2 ;;
esac
REPO_REF="${IMAGE%:*}"
# ghcr.io/<owner>/guardian -> <owner>/guardian; override with --repo when the
# registry path and the GitHub repo name diverge.
[ -n "$GH_REPO" ] || GH_REPO="${REPO_REF#ghcr.io/}"

echo ">> Resolving $IMAGE"
SHA="$(docker buildx imagetools inspect "$IMAGE" --format '{{.Manifest.Digest}}')"
[ -n "$SHA" ] || { echo "could not resolve $IMAGE; has the publish workflow run?" >&2; exit 1; }
DIGEST="$REPO_REF@$SHA"
echo ">> Digest $DIGEST"

# The attestation is what links these bytes to a commit. Without it the compose
# hash pins bytes of unknown origin, which is the thing the on-chain approval is
# supposed to rule out.
echo ">> Verifying build provenance against $GH_REPO"
gh attestation verify "oci://$DIGEST" --repo "$GH_REPO"

python3 - "$COMPOSE" "$DIGEST" <<'PY'
import re, sys
path, digest = sys.argv[1], sys.argv[2]
text = open(path).read()
new, count = re.subn(r'^(\s*image:\s*).*$', lambda m: m.group(1) + digest, text, count=1, flags=re.M)
if count != 1:
    sys.exit(f"expected exactly one image line in {path}, replaced {count}")
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

echo ">> Deploying to CVM $CVM_ID"
phala deploy --cvm-id "$CVM_ID" --compose "$COMPOSE" --wait

echo
echo ">> Deployed. If this app uses Onchain KMS, the new compose_hash must be"
echo ">> registered with addComposeHash before the CVM can obtain its keys."
