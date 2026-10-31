# Maintainer: Ben Kepner <u6bkep@gmail.com>
# repoman builds itself from this branch and serves the result from its own
# repo, so each machine installs the worker with pacman.
pkgname=repoman-git
pkgver=r0
pkgrel=1
pkgdesc="AUR build coordinator, worker, and CLI with isolated builds and signed per-stream repos"
arch=('x86_64')
url="https://github.com/u6bkep/repoman"
license=('MIT')
depends=('pacman' 'git' 'gnupg' 'util-linux' 'libarchive' 'zstd' 'coreutils' 'iproute2')
makedepends=('cargo' 'git')
optdepends=('arch-install-scripts: not required; images are bootstrapped with pacman')
provides=('repoman')
conflicts=('repoman')
backup=('etc/repoman/worker.toml')
source=("repoman::git+https://github.com/u6bkep/repoman.git#branch=v2-main")
sha256sums=('SKIP')

pkgver() {
    cd repoman
    printf "r%s.%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

prepare() {
    cd repoman
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd repoman
    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    cargo build --frozen --release --bin repoman
}

check() {
    cd repoman
    export RUSTUP_TOOLCHAIN=stable
    # Unit tests only: the integration tests need network and repo-add.
    cargo test --frozen --release --workspace --lib
}

package() {
    cd repoman
    install -Dm755 target/release/repoman "$pkgdir/usr/bin/repoman"
    install -Dm644 deploy/repoman-worker.service "$pkgdir/usr/lib/systemd/system/repoman-worker.service"
    install -Dm600 deploy/worker.example.toml "$pkgdir/etc/repoman/worker.toml"
    install -Dm644 deploy/coordinator.example.toml "$pkgdir/usr/share/doc/repoman/coordinator.example.toml"
    install -Dm644 deploy/README.md "$pkgdir/usr/share/doc/repoman/README.md"
    install -Dm644 docs/PLAN.md "$pkgdir/usr/share/doc/repoman/PLAN.md"
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE" 2>/dev/null || true
}
