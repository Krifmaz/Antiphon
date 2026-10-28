# Self-hosted Stoop relay (spec §3: "Community owners can self-host").
# Build from the repository root:
#   docker build -f crates/relay/Dockerfile -t antiphon-relay .
#   docker run -p 3479:3479/udp antiphon-relay
FROM rust:1.97-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p antiphon-relay

FROM debian:bookworm-slim
COPY --from=build /src/target/release/antiphon-relay /usr/local/bin/antiphon-relay
EXPOSE 3479/udp
USER nobody
ENTRYPOINT ["antiphon-relay", "--listen", "0.0.0.0:3479"]
