FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --no-install-recommends -y bash ca-certificates zsh \
    && rm -rf /var/lib/apt/lists/*

COPY target/release/command-api /usr/local/bin/command-api

ENV RUST_LOG=command_api=info,tower_http=info

EXPOSE 27415
USER 65532:65532

ENTRYPOINT ["/usr/local/bin/command-api", "run", "--config", "/etc/command-api/config.yaml"]
