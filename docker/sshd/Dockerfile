# Minimal SSH server for Elum integration tests.
# Alpine + openssh-server + a single key-auth-only test user.

FROM alpine:3.20

RUN apk add --no-cache openssh-server bash \
 && ssh-keygen -A \
 && adduser -D -s /bin/bash testuser \
 && passwd -d testuser \
 && mkdir -p /home/testuser/.ssh \
 && chmod 700 /home/testuser/.ssh

COPY fixtures/id_ed25519.pub /home/testuser/.ssh/authorized_keys

RUN chown -R testuser:testuser /home/testuser/.ssh \
 && chmod 600 /home/testuser/.ssh/authorized_keys

# Lock down sshd: no passwords, no root, single allowed user.
RUN printf '%s\n' \
    'PermitRootLogin no' \
    'PasswordAuthentication no' \
    'ChallengeResponseAuthentication no' \
    'PubkeyAuthentication yes' \
    'AllowUsers testuser' \
    'AcceptEnv LANG LC_*' \
    'PrintMotd no' \
    > /etc/ssh/sshd_config.d/elum.conf

EXPOSE 22

# `-D` keeps sshd in foreground so the container stays up.
# `-e` logs to stderr (visible via `docker logs`).
CMD ["/usr/sbin/sshd", "-D", "-e"]
