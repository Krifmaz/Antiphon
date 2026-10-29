# Headless Antiphon test peer: `antiphon-cli echo` answers every call and
# sends the caller's audio straight back. See docker/README.md.
#
# Antiphon has no server in the call path; this is a second peer that can
# live on a box you can reach, so a desktop (or phone) client always has
# someone to call.

# BUILDING.md's stated toolchain, matching CI.
ARG RUST_VERSION=1.97.0

FROM rust:${RUST_VERSION}-slim-trixie AS build
# The CLI links ALSA (cpal) and X11 (push-to-talk key polling) even though
# `echo` uses neither, so their headers are needed to build it.
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libasound2-dev libx11-dev libxi-dev libxtst-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p antiphon-cli \
 && cp target/release/antiphon-cli /antiphon-cli

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends libasound2t64 libx11-6 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 antiphon \
 && mkdir /data && chown antiphon /data
COPY --from=build /antiphon-cli /usr/local/bin/antiphon-cli
USER antiphon
# The identity key lives here; mount a volume to keep it across restarts,
# or clients see a key-change warning every time the container is recreated.
ENV XDG_CONFIG_HOME=/data
VOLUME /data
EXPOSE 5000/udp
ENTRYPOINT ["antiphon-cli"]
CMD ["echo", "--port", "5000"]
