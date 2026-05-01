# Test SSH key - DO NOT REUSE

The `id_ed25519` / `id_ed25519.pub` pair in this directory is **committed to
the repository on purpose**. It exists solely to authenticate against the
local `sshd` container defined in `../Dockerfile` for `elum-ssh` integration
tests.

## Why is this safe?

- The container only listens on `127.0.0.1:2222` (loopback, never the LAN).
- The key is ed25519 with a comment of `elum-test-only-DO-NOT-REUSE`.
- The container is a sealed Alpine box with one unprivileged user
  (`testuser`) and a locked password (`passwd -d`).
- The key has never been deployed to any real host and never will be.

## Do not

- Do not copy this private key to any real server's `authorized_keys`.
- Do not regenerate it with the same comment and reuse it elsewhere.

## Regeneration (rare)

If the key ever needs to be rotated:

```bash
rm docker/sshd/fixtures/id_ed25519 docker/sshd/fixtures/id_ed25519.pub
ssh-keygen -t ed25519 -N "" \
  -C "elum-test-only-DO-NOT-REUSE" \
  -f docker/sshd/fixtures/id_ed25519
docker compose -f docker/sshd/compose.yml build --no-cache
```
