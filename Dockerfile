FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev ca-certificates
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY web web
RUN cargo build --release --locked

FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /app/target/release/house-panel /house-panel
# Listed so container UIs show the settings with their defaults.
ENV DATA=/data \
    FLOORPLAN=/floorplan \
    PORT=8770
EXPOSE 8770
ENTRYPOINT ["/house-panel"]
