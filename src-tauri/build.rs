use std::process::Command;

fn main() {
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .or_else(|| std::env::var("ST_GIT_HASH").ok())
        .unwrap_or_else(|| "unknown".to_string());

    let build_date = std::env::var("ST_BUILD_DATE").unwrap_or_else(|_| {
        Command::new("date")
            .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout).ok().map(|s| s.trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "unknown".to_string())
    });

    // MERIDIAN strategy artefact hash (plan §0.0.1, ANALIZA chat 3 L20): the
    // paper counter runs on the *strategy* — the pure crate plus the host
    // module — not on the whole repository, so UI work, legacy removals and
    // the daemon do not reset it. FNV-1a 64 over (relative path, contents)
    // of every file, sorted; no build dependency, stable across toolchains.
    // `daemon.rs` (process plumbing: lease, state file, viewer, signals) is
    // not part of the strategy and is excluded on purpose.
    let strategy_hash = strategy_hash(&["crates/titan-meridian/src", "src/meridian"], &["src/meridian/daemon.rs"]);

    println!("cargo:rustc-env=ST_GIT_HASH={}", git_hash);
    println!("cargo:rustc-env=ST_BUILD_DATE={}", build_date);
    println!("cargo:rustc-env=ST_MERIDIAN_HASH={}", strategy_hash);
    println!("cargo:rerun-if-changed=src/");
    println!("cargo:rerun-if-changed=crates/titan-meridian/src/");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/");

    tauri_build::build()
}

fn strategy_hash(roots: &[&str], exclude: &[&str]) -> String {
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for root in roots {
        collect_files(std::path::Path::new(root), &mut files);
    }
    files.retain(|f| !exclude.iter().any(|x| std::path::Path::new(x) == f));
    files.sort();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for f in &files {
        feed(f.to_string_lossy().replace('\\', "/").as_bytes());
        feed(&[0]);
        feed(&std::fs::read(f).unwrap_or_default());
        feed(&[0]);
    }
    format!("{h:016x}")[..12].to_string()
}

fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}
