# Deploying guardian to a Phala Cloud CVM

Runs the guardian server inside an Intel TDX confidential VM. The storage
encryption key is derived by the dstack guest agent and bound to the
application's on-chain identity, so whoever holds the Postgres credentials
reads only ciphertext.

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
| `GUARDIAN_ACK_SECRET_PROVIDER` | `aws` |
| `AWS_REGION` | `us-east-1` |
| `GUARDIAN_MIDEN_RPC_ENDPOINT` | the Leviathan node |

Anything that decides *what runs* or *which chain it talks to* belongs inline,
not in a variable. dstack hashes this file into `compose_hash` and that hash is
what gets approved on chain, so a value left outside the file is a value the
approval does not pin. The image is therefore pinned by content digest
(`@sha256:...`), never by tag: a tag can be repointed at different bytes while
the compose file, and so the approved hash, stays identical.

Supplied through encrypted env:

| Variable | Notes |
|---|---|
| `DATABASE_URL` | Postgres lives **outside** the CVM |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | for the ACK keys |
| `GUARDIAN_ACK_FALCON_SECRET_ID`, `GUARDIAN_ACK_ECDSA_SECRET_ID` | Secrets Manager ids |
| `GUARDIAN_DASHBOARD_CURSOR_SECRET` | needed once there is more than one replica, so dashboard cursors validate across them; a missing one only warns and boots with a per-process random secret |

Two things are deliberately unset, and both have consequences worth choosing
rather than discovering: without `GUARDIAN_CORS_ALLOWED_ORIGINS` the server
falls back to allowing any origin, and without an operator allowlist
(`GUARDIAN_OPERATOR_PUBLIC_KEYS_FILE` or `..._SECRET_ID`) nobody can
authenticate to `/dashboard/*`.

`GUARDIAN_ACK_SECRET_PROVIDER` must not be `none`: that generates a fresh ack
keypair on every boot, which changes the on-chain ack-key commitment and
freezes any account pinned to the previous one.

The ACK keys stay in AWS rather than dstack. They are signing keys, not
confidentiality keys - an operator holding one still cannot read account state
and still cannot move funds, because on-chain auth needs the user's signatures.

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
./scripts/deploy-phala.sh ghcr.io/<owner>/guardian:<tag> --cvm-id <id>
```

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
