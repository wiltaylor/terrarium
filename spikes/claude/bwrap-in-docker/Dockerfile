# Scratch image for the bwrap-in-docker spike.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends bubblewrap ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -m -u 1000 agent
COPY inside.sh /usr/local/bin/inside.sh
RUN chmod 0755 /usr/local/bin/inside.sh
USER agent
WORKDIR /home/agent
CMD ["/usr/local/bin/inside.sh"]
