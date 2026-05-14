# Maintainer: Ben Kepner <u6bkep@gmail.com>
pkgname=repoman
pkgver=0.1.0
pkgrel=1
pkgdesc="AUR package repository manager with containerized builds and web interface"
arch=('x86_64')
url="https://github.com/u6bkep/repoman"
license=('MIT')
depends=('podman' 'git' 'pacman')
makedepends=('cargo' 'rust')
optdepends=(
    'nginx: serve repository over HTTP'
    'nfs-utils: share repository over NFS'
)
backup=('etc/repoman/config.toml')
source=("$pkgname-$pkgver.tar.gz")
sha256sums=('SKIP')

prepare() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    cargo build --frozen --release
}

check() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo test --frozen
}

package() {
    cd "$pkgname-$pkgver"

    # Install binary (static assets are embedded)
    install -Dm755 "target/release/$pkgname" "$pkgdir/usr/bin/$pkgname"

    # Install default config
    install -Dm644 /dev/stdin "$pkgdir/etc/repoman/config.toml" << 'EOF'
[repo]
path = "/srv/repo/aur"
name = "aur-local"
keep_versions = 2

[cache]
aur_clones = "/var/cache/repoman/aur"
pacman_cache = "/var/cache/repoman/pacman"
logs = "/var/log/repoman"

[container]
image = "repoman-builder"

[server]
listen = "127.0.0.1:8080"
EOF

    # Install systemd service
    install -Dm644 /dev/stdin "$pkgdir/usr/lib/systemd/system/repoman.service" << 'EOF'
[Unit]
Description=Repoman AUR Repository Manager
After=network.target

[Service]
Type=simple
User=repoman
Group=repoman
ExecStart=/usr/bin/repoman serve
Restart=on-failure
RestartSec=5

# Security hardening
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
ReadWritePaths=/srv/repo /var/cache/repoman /var/log/repoman

[Install]
WantedBy=multi-user.target
EOF

    # Install sysusers.d config
    install -Dm644 /dev/stdin "$pkgdir/usr/lib/sysusers.d/repoman.conf" << 'EOF'
u repoman - "Repoman AUR Manager" /var/cache/repoman
EOF

    # Install tmpfiles.d config
    install -Dm644 /dev/stdin "$pkgdir/usr/lib/tmpfiles.d/repoman.conf" << 'EOF'
d /srv/repo/aur 0755 repoman repoman -
d /var/cache/repoman 0755 repoman repoman -
d /var/cache/repoman/aur 0755 repoman repoman -
d /var/cache/repoman/pacman 0755 repoman repoman -
d /var/log/repoman 0755 repoman repoman -
EOF

    # Install license
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE" 2>/dev/null || true

    # Install documentation
    install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md" 2>/dev/null || true
    install -Dm644 DESIGN.md "$pkgdir/usr/share/doc/$pkgname/DESIGN.md" 2>/dev/null || true
}
