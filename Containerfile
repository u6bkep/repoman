# Stage 1: Build
FROM docker.io/library/rust:latest AS builder

WORKDIR /build

# Cache dependency build layer
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Build real binary with embedded static assets
COPY src/ src/
COPY static/ static/
RUN touch src/main.rs && cargo build --release

# Stage 2: Runtime
FROM docker.io/archlinux/archlinux:base-devel

# Install runtime dependencies
RUN pacman -Syu --noconfirm \
        podman fuse-overlayfs slirp4netns git pacman-contrib && \
    pacman -Scc --noconfirm

# Create repoman user with subuid/subgid for rootless podman
RUN useradd -m -s /bin/bash repoman && \
    echo "repoman:100000:65536" >> /etc/subuid && \
    echo "repoman:100000:65536" >> /etc/subgid

# Configure rootless podman storage
RUN mkdir -p /home/repoman/.config/containers && \
    printf '[storage]\ndriver = "overlay"\n\n[storage.options.overlay]\nmount_program = "/usr/bin/fuse-overlayfs"\n' \
        > /home/repoman/.config/containers/storage.conf && \
    chown -R repoman:repoman /home/repoman/.config

# Create data directories
RUN mkdir -p /data/repoman /srv/repo/aur \
        /var/cache/repoman/aur /var/cache/repoman/pacman \
        /var/cache/repoman/sources /var/log/repoman && \
    chown -R repoman:repoman /data /srv/repo/aur \
        /var/cache/repoman /var/log/repoman

# Copy binary from build stage
COPY --from=builder /build/target/release/repoman /usr/bin/repoman

# Embed default container config
RUN mkdir -p /etc/repoman && \
    printf '[repo]\npath = "/srv/repo/aur"\nname = "aur"\nkeep_versions = 2\n\n[cache]\naur_clones = "/var/cache/repoman/aur"\npacman_cache = "/var/cache/repoman/pacman"\nlogs = "/var/log/repoman"\nsources = "/var/cache/repoman/sources"\n\n[container]\nimage = "repoman-builder"\n\n[server]\nlisten = "0.0.0.0:8080"\n' \
        > /etc/repoman/config.toml

ENV XDG_DATA_HOME=/data

USER repoman
EXPOSE 8080

ENTRYPOINT ["/usr/bin/repoman"]
CMD ["--config", "/etc/repoman/config.toml", "serve"]
