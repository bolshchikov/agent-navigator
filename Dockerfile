# syntax=docker/dockerfile:1
FROM --platform=linux/amd64 cgr.dev/chainguard/rust:latest-dev AS build
USER root
RUN apk add --no-cache cmake
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# rustls only — static libgcc so the runtime image does not need libgcc_s.
ENV RUSTFLAGS="-C link-arg=-static-libgcc"
RUN cargo build --release --locked --bin agent-navigator

FROM --platform=linux/amd64 cgr.dev/chainguard/static:latest AS certs

FROM --platform=linux/amd64 cgr.dev/chainguard/glibc-dynamic:latest
COPY --from=certs /etc/ssl/certs /etc/ssl/certs
COPY --from=build /src/target/release/agent-navigator /usr/local/bin/agent-navigator
USER root
ENV AGENT_NAVIGATOR_SESSION_DIR=/var/data/agent-navigator/sessions
ENV RUST_LOG=info
EXPOSE 10000
ENTRYPOINT ["/usr/local/bin/agent-navigator"]
CMD ["mcp", "--http"]
