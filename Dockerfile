# The `terrio` command-line tool, as release-build.yml builds it: a static musl binary, in a small
# image with a shell for scripts. The binary carries its own TLS roots, so the image needs no CA
# certificates for remote data. Published to ghcr.io/opengeos/terrio by docker.yml.
FROM rust:1-alpine AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY . .
# Without the symbol table, about a quarter of the binary.
ENV CARGO_PROFILE_RELEASE_STRIP=symbols
RUN cargo build --release --locked -p terrio-cli

FROM alpine:3
COPY --from=build /src/target/release/terrio /usr/local/bin/terrio
COPY LICENSE NOTICE LICENSE-THIRD-PARTY.md /usr/share/licenses/terrio/
RUN mkdir /data && chown 65532:65532 /data
WORKDIR /data
USER 65532:65532
ENTRYPOINT ["terrio"]
CMD ["--help"]
