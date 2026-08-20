# Deploying guardian to a Phala Cloud CVM

Runs the guardian server inside an Intel TDX confidential VM. The storage
encryption key is derived by the dstack guest agent and bound to the
application's on-chain identity, so a database dump that ever leaves the CVM is
ciphertext.

## Why the compose file must not contain secrets

Phala hashes this file into `compose_hash` and that hash is registered on
chain, where anyone can read it. Every secret-bearing value is therefore a
`${VAR}` reference supplied through Phala Cloud's encrypted environment, never
a literal.

## What the server needs

Non-secret, set inline in the compose file:

| Variable | Value |
|---|---|
| `GUARDIAN_ENV` | `prod` |
| `GUARDIAN_NETWORK_TYPE` | `MidenTestnet` |
| `GUARDIAN_KEYSTORE_PATH` | `/var/guardian/keystore` |
| `GUARDIAN_STORAGE_ENCRYPTION_DSTACK_PATH` | `guardian/storage` |
| `GUARDIAN_ACK_SECRET_PROVIDER` | `file` |
| `GUARDIAN_ACK_FALCON_SECRET_PATH`, `GUARDIAN_ACK_ECDSA_SECRET_PATH` | under `/var/guardian/keystore` |
| `GUARDIAN_ACK_SECRET_AUTOGEN` | `true` |
| `GUARDIAN_MIDEN_RPC_ENDPOINT` | the Leviathan node |
| `DATABASE_URL` | host, user and database name; only the password is a variable |

Anything that decides *what runs* or *which chain it talks to* belongs inline,
not in a variable. dstack hashes this file into `compose_hash` and that hash is
what gets approved on chain, so a value left outside the file is a value the
approval does not pin. The image is therefore pinned by content digest
(`@sha256:...`), never by tag: a tag can be repointed at different bytes while
the compose file, and so the approved hash, stays identical.

Supplied through encrypted env:

| Variable | Notes |
|---|---|
| `POSTGRES_PASSWORD` | the only required one; read by both services, and the database is never reachable from outside. It is spliced into `DATABASE_URL`, so keep it URL-safe (`openssl rand -hex 32`). Postgres applies it at initdb only: once the volume exists, changing it here just breaks guardian's login, and rotating for real needs an `ALTER ROLE` inside the CVM |
| `GUARDIAN_DASHBOARD_CURSOR_SECRET` | optional; needed once there is more than one replica, so dashboard cursors validate across them. A missing one only warns and boots with a per-process random secret |

Two things are deliberately unset, and both have consequences worth choosing
rather than discovering: without `GUARDIAN_CORS_ALLOWED_ORIGINS` the server
falls back to allowing any origin, and without an operator allowlist
(`GUARDIAN_OPERATOR_PUBLIC_KEYS_FILE` or `..._SECRET_ID`) nobody can
authenticate to `/dashboard/*`.

## Where the ACK keys come from

`GUARDIAN_ACK_SECRET_PROVIDER=file` plus `GUARDIAN_ACK_SECRET_AUTOGEN=true`: the
first boot mints a Falcon and an ECDSA key onto the `guardian-keystore` volume,
every boot after reads them back. The keys are generated inside the CVM and never
leave it, so unlike the AWS path there is no copy for anyone to hold - and no AWS
account in the deployment at all.

The cost of that is stated plainly: **losing the volume loses the identity.**
Accounts pinned to the old ack-key commitment then need a per-account
`SwitchGuardian`. Restarts and upgrades keep the volume; deleting the CVM does not.

Autogeneration is off by default everywhere else, because a mistyped path or an
unmounted volume would otherwise mint a fresh identity and freeze those accounts
while looking like a clean boot. Every boot logs an `ack signers` line carrying
`falcon_commitment` and `ecdsa_commitment`, which is how a key that moved gets
noticed; minting itself logs a warning naming the file.

`GUARDIAN_ACK_SECRET_PROVIDER` must not be `none`: that generates a fresh ack
keypair on every boot and never reads one back, so the commitment changes every
restart.

The keys sit on the volume rather than being derived from the dstack agent on
every boot. Deriving them would remove the last stored copy, but it would tie the
Falcon key to `miden-crypto`'s key generation staying byte-stable across upgrades:
a dependency bump that reordered its use of randomness would mint a different key
from the same seed and freeze accounts silently. Stored bytes have no such failure
mode.

## Why Postgres runs inside the CVM

It is a second service in the same compose file, on a Docker volume. The CVM's
data disk is encrypted and its key is released only to a CVM whose `compose_hash`
is approved on chain - the same gate as the storage key above. The volume survives
restarts and in-place upgrades of the same CVM; deleting or re-provisioning the
CVM loses it. The service publishes no ports, and every port published in this
file becomes a public endpoint.

Running it outside would mean a Postgres exposed to the internet. Encrypting the
records does not make that safe: an attacker who cannot read `state_json` can
still delete rows, and the routing columns are plaintext by design.

Application-level encryption still earns its place, because it covers the case
the platform does not: a `pg_dump` copied out of the CVM for backup stays
unreadable to whoever holds it.


## Build, pin and deploy

The image is built by `.github/workflows/docker-publish.yml`, never locally.
That workflow signs the resulting digest with GitHub's identity, so the bytes
running in the CVM can be traced back to a commit by anyone, without trusting
whoever pressed deploy. A hand-built image carries no such signature.

Dispatch it with `features: postgres,tee` and `platforms: linux/amd64` (Phala
runs Intel TDX; an arm64 leg only adds a slow QEMU build nobody deploys). The
`tee` feature is not the default because it pulls the dstack SDK and its
Ethereum dependency stack, which no other deployment needs. A build without it
refuses to start when `GUARDIAN_STORAGE_ENCRYPTION_DSTACK_PATH` is set, rather
than booting with encryption silently disabled.

Building from this fork needs a `GITEA_TOKEN` repository secret - the workspace
pins its Miden crates to revisions in private `git.softly.com` repos.

Then:

```bash
./scripts/deploy-phala.sh ghcr.io/<owner>/guardian:<tag> \
  --repo <owner>/<repo> --cvm-id <id>
```

`--repo` is the GitHub repository the attestation must name. It is required
rather than derived from the registry path, which is not always the same name.

It resolves the tag to a content digest, checks the digest's attestation with
`gh attestation verify`, writes the digest into the compose file, and refuses to
deploy while the placeholder digest is still there. Add `--dry-run` to stop
after pinning. Do not hand-edit the digest.

Verifying independently:

```bash
gh attestation verify oci://ghcr.io/<owner>/guardian@sha256:... --repo <owner>/guardian
```

The GHCR package must be readable by Phala Cloud, so either make it public or
give the CVM registry credentials.

## The approval gate

Under Onchain KMS every deploy needs the new `compose_hash` registered before
the CVM can get its keys:

1. Provision, so the backend computes `compose_hash` from this file.
2. Send `addComposeHash` to the `DstackApp` contract from the owner wallet.
3. Commit, which creates the CVM.

A hash that is not on that list boots into a CVM that cannot obtain the storage
key, so the server refuses to start rather than running unencrypted. Since the
image digest is part of the file, every code change produces a new hash and
needs step 2 again - that is the gate working, not friction to design around.

## Verifying it is actually working

```sql
SELECT account_id, commitment, state_json FROM states LIMIT 1;
```

`state_json` must be an AES-256-GCM envelope with `"kid":"dstack:guardian/storage"`.
`account_id` and `commitment` stay plaintext by design - they are routing
fields, and they are bound into the ciphertext as AEAD additional data.
