//! Parse one or more sync databases and print a summary. Used to validate
//! the reader against real mirrors:
//!
//!     cargo run -p repoman-alpm --example dump_db -- core.db extra.db [name...]

use repoman_alpm::DepSpec;
use repoman_alpm::syncdb::{SyncIndex, read_db};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dbs, names): (Vec<&String>, Vec<&String>) = args.iter().partition(|a| a.contains(".db") || a.contains(".files"));
    let mut idx = SyncIndex::new();
    for path in dbs {
        let bytes = std::fs::read(path)?;
        let start = std::time::Instant::now();
        let pkgs = read_db(&bytes)?;
        let with_files = pkgs.iter().filter(|p| !p.files.is_empty()).count();
        let soname_provides = pkgs
            .iter()
            .flat_map(|p| p.provides.iter())
            .filter(|p| p.contains(".so"))
            .count();
        println!(
            "{path}: {} packages, {} with file lists, {} soname provides, parsed in {:?}",
            pkgs.len(),
            with_files,
            soname_provides,
            start.elapsed()
        );
        let repo = std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("repo")
            .to_string();
        idx.add_repo(&repo, pkgs);
    }
    for n in names {
        let dep = DepSpec::parse(n);
        let sat = idx.satisfiers(&dep);
        if sat.is_empty() {
            println!("{n}: no satisfier");
        }
        for p in sat {
            println!(
                "{n}: {} {} [{}] depends={} provides={}",
                p.name,
                p.version,
                idx.repo_of(&p.name).unwrap_or("?"),
                p.depends.len(),
                p.provides.len()
            );
        }
    }
    Ok(())
}
