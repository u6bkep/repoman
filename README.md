# Repoman

An AUR (Arch User Repository) package repository manager. Repoman tracks AUR packages, provides a PKGBUILD review workflow, runs isolated builds in rootless Podman containers, and serves a web interface for management.

> **Project status:** personal project under active development. The CLI and web interface are usable but APIs, schemas, and storage layouts may still shift between commits. Feedback and issues are welcome, but expect rough edges.

## Features

- **PKGBUILD Review Workflow**: Review and approve changes before building
- **Isolated Container Builds**: Rootless Podman for secure, reproducible builds
- **Dependency Tracking**: Automatic AUR dependency resolution with orphan detection
- **Split Package Support**: Handles PKGBUILDs that produce multiple packages
- **Web Interface**: Modern SPA for package management
- **REST API**: Full API access for automation
- **Update Checking**: Monitor AUR for upstream changes

## Requirements

- Rust 1.85+ (uses edition 2024)
- Podman (rootless)
- SQLite

## Installation

```bash
# Clone
git clone https://github.com/u6bkep/repoman.git
cd repoman

# Build from source
cargo build --release

# Binary will be at ./target/release/repoman
```

A `PKGBUILD` is also included for Arch users who want to install via `makepkg`.

### Running in a container (work in progress)

Containerized deployment lives under [`docker/`](docker/README.md). It works
but isn't the recommended way to run repoman yet — building and running
directly on the host is the supported path.

## Quick Start

```bash
# Add a package from AUR
repoman add firefox

# Review the PKGBUILD
repoman diff firefox
repoman review firefox    # Opens in $EDITOR

# Approve and build
repoman approve firefox
repoman build firefox

# Start the web interface
repoman serve
# Open http://localhost:8080
```

## Configuration

Configuration file: `~/.config/repoman/config.toml`

```toml
[repo]
path = "/srv/repo/aur"           # Repository directory
name = "aur-local"               # Repository name (for pacman.conf)
keep_versions = 2                # Old versions to keep

[cache]
aur_clones = "~/.cache/repoman/aur"
pacman_cache = "~/.cache/repoman/pacman"
logs = "~/.local/share/repoman/logs"
sources = "~/.cache/repoman/sources"

[container]
image = "repoman-builder"        # Base image name
tmpfs_build = false              # Use ramdisk for builds (faster, uses RAM)
tmpfs_size = "8G"                # Size limit for tmpfs

[server]
listen = "127.0.0.1:8080"        # Web server bind address
```

Generate a template config:
```bash
repoman config template > ~/.config/repoman/config.toml
```

### Environment Variables

All config options can be overridden with environment variables:

- `REPOMAN_REPO_PATH`
- `REPOMAN_REPO_NAME`
- `REPOMAN_REPO_KEEP_VERSIONS`
- `REPOMAN_CACHE_AUR_CLONES`
- `REPOMAN_CACHE_PACMAN`
- `REPOMAN_CACHE_LOGS`
- `REPOMAN_CACHE_SOURCES`
- `REPOMAN_CONTAINER_IMAGE`
- `REPOMAN_CONTAINER_TMPFS_BUILD`
- `REPOMAN_CONTAINER_TMPFS_SIZE`
- `REPOMAN_SERVER_LISTEN`

## CLI Commands

### Package Management

```bash
repoman add <package>              # Add package (explicit install)
repoman add --asdeps <package>     # Add as dependency
repoman add --with-deps <package>  # Add with AUR dependencies

repoman remove <package>           # Remove package
repoman remove -s <package>        # Remove + orphaned deps (like pacman -Rs)

repoman list                       # List all packages
repoman list --explicit            # Explicitly installed only
repoman list --deps                # Dependencies only
repoman list --orphans             # Orphaned dependencies
repoman list --status <status>     # Filter by status

repoman info <package>             # Show package details

repoman mark --explicit <pkg>      # Mark as explicitly installed
repoman mark --asdeps <pkg>        # Mark as dependency
```

### Review Workflow

```bash
repoman diff <package>             # Show changes since last approval
repoman review <package>           # Open in $EDITOR for review
repoman review --stdout <package>  # Print to stdout
repoman approve <package>          # Approve current commit
```

### Build Operations

```bash
repoman build <package>            # Build single package
repoman build <pkg1> <pkg2>        # Build multiple (dependency order)
repoman build --all                # Build all approved packages
repoman build --updates            # Build packages with updates

repoman logs <build-id>            # View build logs
repoman logs <build-id> -f         # Follow logs (like tail -f)
repoman logs <build-id> -n 100     # Show last N lines
```

### Maintenance

```bash
repoman check-updates              # Check all packages for updates
repoman rebuild-image              # Rebuild base container image
repoman status                     # System status overview
repoman config show                # Show effective configuration
repoman import <db.tar.zst>        # Import from existing repo
repoman import --dry-run <db>      # Preview import
```

All commands support `--json` for machine-readable output.

## Package Status Workflow

```
PendingReview → Approved → Building → UpToDate
     ↓              ↓                    ↓
  Rejected     UpdateAvailable        Failed
```

- **PendingReview**: Newly added, PKGBUILD needs approval
- **Approved**: Ready to build
- **Building**: Currently building
- **UpToDate**: Successfully built and in repository
- **UpdateAvailable**: Upstream has changes
- **Failed**: Build failed (check logs)
- **Rejected**: User rejected PKGBUILD

## Install Reason Tracking

Like `pacman`, Repoman tracks why packages were installed:

- **explicit**: User directly added the package
- **dependency**: Added automatically as a dependency

This enables orphan detection:
```bash
repoman list --orphans    # Show deps with no dependents
repoman remove -s <pkg>   # Remove package + orphaned deps
```

## Web Interface

Start the server:
```bash
repoman serve
```

The web interface provides:

- **Dashboard**: Overview of repository status and packages needing attention
- **Package List**: Filterable, sortable list with quick actions
- **Package Detail**: PKGBUILD viewer, diff view, build history
- **Add Package**: AUR search with dependency preview
- **Build Logs**: Syntax-highlighted with live streaming
- **Settings**: System status and maintenance tools

## Using Your Repository

Add to `/etc/pacman.conf`:

```ini
[aur-local]
SigLevel = Optional TrustAll
Server = file:///srv/repo/aur
```

Then install packages:
```bash
sudo pacman -Sy
sudo pacman -S firefox
```

## Directory Structure

```
~/.config/repoman/
└── config.toml              # Configuration

~/.cache/repoman/
├── aur/                     # Git clones
├── pacman/                  # Downloaded package cache
└── sources/                 # Downloaded sources

~/.local/share/repoman/
├── repoman.db               # SQLite database
└── logs/                    # Build logs

/srv/repo/aur/               # Repository (configurable)
├── aur.db.tar.zst
├── aur.files.tar.zst
└── *.pkg.tar.zst
```

## Troubleshooting

### Permission issues with container builds
Rootless Podman creates files with subuid ownership. Clean up with:
```bash
podman unshare rm -rf /path/to/files
```

### Package won't build
```bash
repoman logs <pkgbase>      # Check full build logs
repoman rebuild-image       # Rebuild base container if corrupted
```

### Reset database
```bash
rm ~/.local/share/repoman/repoman.db
# Database recreates on next run
```

### Config issues
```bash
repoman config show         # Show effective configuration
repoman config path         # Find config file location
```

## Contributing

Issues and pull requests are welcome. For larger changes please open an issue first to discuss the approach, since the internals are still in flux.

## License

[MIT](LICENSE) © Ben Kepner
