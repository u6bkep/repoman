# Coordinator image. Built from the repository root:
#   docker build -f deploy/Dockerfile -t repoman:v2 .
#
# Runtime is Arch so pacstrap, repo-add, and friends are available to the
# base-image builder and publisher in M1.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked --bin repoman

FROM archlinux:base
RUN pacman -Syu --noconfirm --needed arch-install-scripts pacman-contrib git \
    && pacman -Scc --noconfirm
COPY --from=build /src/target/release/repoman /usr/local/bin/repoman
VOLUME ["/var/lib/repoman"]
EXPOSE 7800
ENTRYPOINT ["/usr/local/bin/repoman"]
CMD ["coordinator", "--config", "/etc/repoman/coordinator.toml"]
