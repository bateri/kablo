# The server of `make e2e`: a stock Debian with an OpenSSH server, a user that
# logs in with a key, and the statically linked kablo binary the build step
# put next to this file. Nothing here is tuned for kablo: `sshd_config` keeps
# MaxSessions at its default (10), which is what makes the "fifteen sessions
# over one connection" scenario mean something, and there is no systemd, so the
# keeper survives the end of the ssh session by the `setsid` route.
#
# The two things that are not stock are for the port forwarding scenarios:
# sshd refuses every kind of forwarding (`DisableForwarding yes`, as a server
# whose administrator or policy forbids it does), and `socat` is the TCP server
# the forwarded connections go to (an echo, on 127.0.0.1:7001, started before
# sshd). Both are this test image's, not the product's.
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server procps socat \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /run/sshd \
    && ssh-keygen -A \
    && useradd --create-home --shell /bin/bash --password '*' tester \
    && mkdir -m 700 /home/tester/.ssh \
    && chown tester:tester /home/tester/.ssh

RUN echo 'DisableForwarding yes' > /etc/ssh/sshd_config.d/kablo-e2e.conf \
    && printf '%s\n' '#!/bin/sh' \
        'socat TCP-LISTEN:7001,bind=127.0.0.1,reuseaddr,fork EXEC:/bin/cat &' \
        'exec /usr/sbin/sshd -D -e' > /usr/local/bin/start \
    && chmod 755 /usr/local/bin/start

COPY --chown=tester:tester authorized_keys /home/tester/.ssh/authorized_keys
# A startup file that talks before the program starts: the handshake has to
# find its signature behind it. (No interactive-shell guard on purpose: the
# stock one would make the file silent for `ssh host command`.)
COPY --chown=tester:tester bashrc /home/tester/.bashrc
COPY kablo /usr/local/bin/kablo
RUN chmod 600 /home/tester/.ssh/authorized_keys && chmod 755 /usr/local/bin/kablo

EXPOSE 22
CMD ["/usr/local/bin/start"]
