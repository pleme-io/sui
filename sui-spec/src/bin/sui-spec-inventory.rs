//! `sui-spec-inventory` — operator-facing substrate introspection.
//!
//! Dumps the typed catalog in either a human-readable table or as
//! JSON, with optional filters by maturity gate or domain name.
//! Lets an operator answer "what does this substrate cover?"
//! without grepping Rust source — and lets tooling consume the
//! catalog mechanically.
//!
//! Usage:
//!
//! ```text
//! sui-spec-inventory                    # human table, every domain
//! sui-spec-inventory --json             # JSON dump
//! sui-spec-inventory --gate Working     # filter by maturity
//! sui-spec-inventory --domain fetcher   # one domain in detail
//! sui-spec-inventory --histogram        # gate-count summary
//! ```

use sui_spec::catalog::{self, MaturityGate, SubstrateDomain};
use sui_spec::lock_file::{self, ParsedLockFile};
use sui_spec::narinfo::{self, ParsedNarInfo};
use sui_spec::registry::{self, RegistryEntry, RegistryScope};
use sui_spec::style::{
    self, body, dim_fg, error, glyph_arrow, glyph_gear, glyph_ok, glyph_snowflake,
    header, ident, info, muted, pending, success, warn, NORD13, NORD15, NORD3, NORD8,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse()?;

    // `--flake-lock <path>` mode short-circuits the catalog
    // listing and emits a Nord-styled flake input summary.
    if let Some(path) = &args.flake_lock {
        emit_flake_lock(path)?;
        return Ok(());
    }

    // `--narinfo <path>` mode parses a single binary-cache narinfo
    // file and emits a Nord-styled record summary.
    if let Some(path) = &args.narinfo_path {
        emit_narinfo(path)?;
        return Ok(());
    }

    // `--registry-resolve <ref>` mode walks the canonical registry
    // precedence chain and shows where each scope would map the ref.
    if let Some(flake_ref) = &args.registry_resolve {
        emit_registry_resolve(flake_ref)?;
        return Ok(());
    }

    let cat = if args.topo {
        catalog::topological_order()?
    } else {
        catalog::load_canonical()?
    };

    if args.histogram {
        emit_histogram(&cat);
        return Ok(());
    }

    let filtered: Vec<&SubstrateDomain> = cat
        .iter()
        .filter(|d| match &args.domain_filter {
            Some(name) => d.name == *name,
            None => true,
        })
        .filter(|d| match &args.gate_filter {
            Some(gate) => &gate_name(d.gate) == gate,
            None => true,
        })
        .collect();

    if filtered.is_empty() {
        eprintln!("no domains match the filter");
        std::process::exit(1);
    }

    if args.json {
        let body = serde_json::to_string_pretty(&filtered)?;
        println!("{body}");
    } else {
        emit_table(&filtered);
    }
    Ok(())
}

struct Args {
    json: bool,
    histogram: bool,
    topo: bool,
    gate_filter: Option<String>,
    domain_filter: Option<String>,
    flake_lock: Option<String>,
    narinfo_path: Option<String>,
    registry_resolve: Option<String>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = std::env::args().skip(1);
        let mut out = Args {
            json: false,
            histogram: false,
            topo: false,
            gate_filter: None,
            domain_filter: None,
            flake_lock: None,
            narinfo_path: None,
            registry_resolve: None,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--json" => out.json = true,
                "--histogram" => out.histogram = true,
                "--topo" => out.topo = true,
                "--gate" => out.gate_filter = Some(args.next()
                    .ok_or("--gate needs value")?),
                "--domain" => out.domain_filter = Some(args.next()
                    .ok_or("--domain needs value")?),
                "--flake-lock" => out.flake_lock = Some(args.next()
                    .ok_or("--flake-lock needs <path>")?),
                "--narinfo" => out.narinfo_path = Some(args.next()
                    .ok_or("--narinfo needs <path>")?),
                "--registry-resolve" => out.registry_resolve = Some(args.next()
                    .ok_or("--registry-resolve needs <ref>")?),
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(out)
    }
}

fn print_help() {
    println!(
        "sui-spec-inventory — typed substrate introspection.\n\n\
         Usage:\n  sui-spec-inventory [options]\n\n\
         Options:\n  \
         --json                 Emit JSON instead of human table\n  \
         --histogram            Emit gate-count summary instead\n  \
         --topo                 Sort topologically (leaves first; implementation order)\n  \
         --gate <name>          Filter by maturity gate (Working | M2TypedOnly | \
                                M3TypedOnly | M4TypedOnly | Informational)\n  \
         --domain <name>        Show one domain (e.g. fetcher, derivation)\n  \
         --flake-lock <path>    Parse a flake.lock and emit a Nord-styled input summary\n  \
         --narinfo <path>       Parse a single .narinfo and emit a Nord-styled record\n  \
         --registry-resolve <ref>   Walk registry precedence (flake-local → user → system → global) for <ref>\n  \
         -h, --help             This message"
    );
}

fn gate_name(gate: MaturityGate) -> String {
    match gate {
        MaturityGate::Working         => "Working".into(),
        MaturityGate::M2TypedOnly     => "M2TypedOnly".into(),
        MaturityGate::M3TypedOnly     => "M3TypedOnly".into(),
        MaturityGate::M4TypedOnly     => "M4TypedOnly".into(),
        MaturityGate::Informational   => "Informational".into(),
    }
}

fn emit_histogram(cat: &[SubstrateDomain]) {
    let hist = catalog::maturity_histogram().expect("histogram must compute");
    let width = 28;
    println!("{}", style::box_top(width, Some("substrate maturity")));
    println!(
        "{} {:<15} {:>6} {}",
        muted("│"),
        body("Gate"),
        body("Count"),
        muted("│"),
    );
    println!("{}", style::box_mid(width));
    for (gate, count) in &hist {
        let label = match *gate {
            "Working" => success(&format!("{:<15}", gate)),
            "M2TypedOnly" | "M3TypedOnly" | "M4TypedOnly" => {
                style::pending(&format!("{:<15}", gate))
            }
            "Informational" => info(&format!("{:<15}", gate)),
            _ => body(&format!("{:<15}", gate)),
        };
        let count_str = body(&format!("{:>6}", count));
        println!("{} {} {} {}", muted("│"), label, count_str, muted("│"));
    }
    println!("{}", style::box_mid(width));
    let total = body(&format!("{:>6}", cat.len()));
    let total_label = ident(&format!("{:<15}", "Total"));
    println!("{} {} {} {}", muted("│"), total_label, total, muted("│"));
    println!("{}", style::box_bottom(width));
}

fn gate_style(gate: MaturityGate, text: &str) -> String {
    match gate {
        MaturityGate::Working => success(text),
        MaturityGate::M2TypedOnly => style::warn(text),
        MaturityGate::M3TypedOnly | MaturityGate::M4TypedOnly => style::pending(text),
        MaturityGate::Informational => info(text),
    }
}

fn emit_table(domains: &[&SubstrateDomain]) {
    let name_w = domains
        .iter()
        .map(|d| d.name.len())
        .max()
        .unwrap_or(10)
        .max(6);
    let gate_w = 13;
    let kw_w = domains
        .iter()
        .map(|d| d.authoring_keywords.join(", ").len())
        .max()
        .unwrap_or(20)
        .min(40);

    let banner = format!(
        "{}  {}  ({} domains)",
        glyph_snowflake(),
        header("sui-spec substrate"),
        ident(&domains.len().to_string()),
    );
    println!("{banner}");
    println!();

    println!(
        "{}  {}  {}  {}",
        body(&format!("{:<name_w$}", "Domain", name_w = name_w)),
        body(&format!("{:<gate_w$}", "Gate", gate_w = gate_w)),
        body(&format!("{:<kw_w$}", "Keyword(s)", kw_w = kw_w)),
        body("Purpose"),
    );
    println!(
        "{}",
        muted(&"─".repeat(name_w + gate_w + kw_w + 30))
    );
    for d in domains {
        let kws = d.authoring_keywords.join(", ");
        let kw_trunc = if kws.len() > kw_w {
            format!("{}…", &kws[..kw_w.saturating_sub(1)])
        } else {
            kws.clone()
        };
        let glyph = match d.gate {
            MaturityGate::Working => glyph_ok(),
            _ => glyph_gear(),
        };
        let _ = glyph;
        println!(
            "{} {}  {}  {}  {}",
            match d.gate {
                MaturityGate::Working => glyph_ok(),
                _ => glyph_gear(),
            },
            ident(&format!("{:<name_w$}", d.name, name_w = name_w - 2)),
            gate_style(d.gate, &format!("{:<gate_w$}", gate_name(d.gate), gate_w = gate_w)),
            dim_fg(NORD15, &format!("{:<kw_w$}", kw_trunc, kw_w = kw_w)),
            body(&d.purpose),
        );
    }
}

// Suppress unused-import warnings for items kept available to
// future extensions of this binary.
#[allow(dead_code)]
fn _unused() {
    let _ = (error("x"), NORD13, NORD3, glyph_arrow(), warn("x"), pending("x"), NORD8);
}

// ── --flake-lock mode ──────────────────────────────────────────────

fn emit_flake_lock(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("reading {path}: {e}"))?;
    let fmt = lock_file::load_canonical()?
        .into_iter()
        .find(|f| f.name == "cppnix-flake-lock-v7")
        .ok_or("missing cppnix-flake-lock-v7 format")?;
    let parsed = lock_file::parse(&text, &fmt)?;
    emit_flake_lock_table(&parsed, path);
    Ok(())
}

fn emit_flake_lock_table(lock: &ParsedLockFile, path: &str) {
    let banner = format!(
        "{}  {}  {}  {}",
        glyph_snowflake(),
        header("flake.lock"),
        muted(path),
        ident(&format!("v{}", lock.version)),
    );
    println!("{banner}");
    println!();

    // Root inputs first — the direct edges.
    let root_inputs = match lock_file::root_inputs(lock) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", error(&format!("error walking root: {e:?}")));
            return;
        }
    };
    println!(
        "{}  {}  {}",
        glyph_arrow(),
        body("root inputs:"),
        ident(&root_inputs.len().to_string()),
    );
    for input_name in &root_inputs {
        let row = format!(
            "  {}  {}  {}",
            success(input_name),
            muted("→"),
            describe_input(lock, input_name),
        );
        println!("{row}");
    }
    println!();

    // Then the full node count + a few stats.
    let total = lock.nodes.len();
    let transitives = total.saturating_sub(1 + root_inputs.len()); // root + direct + rest
    println!(
        "{}  {} nodes total  ({} direct, {} transitive)",
        muted("∑"),
        body(&total.to_string()),
        ident(&root_inputs.len().to_string()),
        info(&transitives.to_string()),
    );
}

/// Produce a one-line description of an input node from its lock entry.
/// Pulls `locked.type` + `locked.owner/repo/rev` when available.
fn describe_input(lock: &ParsedLockFile, name: &str) -> String {
    let Some(node) = lock.nodes.get(name) else {
        return muted("(missing in nodes)").to_string();
    };
    let Some(locked) = node.get("locked").and_then(|v| v.as_object()) else {
        return muted("(no locked metadata)").to_string();
    };
    let kind = locked.get("type").and_then(|v| v.as_str()).unwrap_or("?");
    let rev = locked.get("rev").and_then(|v| v.as_str());
    let owner = locked.get("owner").and_then(|v| v.as_str());
    let repo = locked.get("repo").and_then(|v| v.as_str());
    let url = locked.get("url").and_then(|v| v.as_str());
    let nar_hash = locked.get("narHash").and_then(|v| v.as_str());

    match (kind, owner, repo, url) {
        ("github", Some(o), Some(r), _) => format!(
            "{} {}{}{}",
            info("github:"),
            body(&format!("{o}/{r}")),
            rev.map(|h| format!(" @{}", &h[..8.min(h.len())])).unwrap_or_default(),
            nar_hash.map(|h| format!(" {}", muted(&truncate(h, 28))))
                .unwrap_or_default(),
        ),
        ("git", _, _, Some(u)) => format!(
            "{} {}{}",
            info("git:"),
            body(u),
            rev.map(|h| format!(" @{}", &h[..8.min(h.len())])).unwrap_or_default(),
        ),
        ("path", _, _, _) => format!(
            "{} {}",
            info("path:"),
            nar_hash.map(|h| muted(&truncate(h, 32)).to_string())
                .unwrap_or_else(|| muted("(no narHash)").to_string()),
        ),
        ("tarball", _, _, Some(u)) => format!(
            "{} {}",
            info("tarball:"),
            body(u),
        ),
        _ => format!(
            "{} {}",
            info(&format!("{kind}:")),
            nar_hash.map(|h| muted(&truncate(h, 32)).to_string())
                .unwrap_or_else(|| muted("(no metadata)").to_string()),
        ),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n.saturating_sub(1)])
    }
}

// ── --narinfo mode ─────────────────────────────────────────────────

fn emit_narinfo(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("reading {path}: {e}"))?;
    let fmt = narinfo::load_canonical()?
        .into_iter()
        .find(|f| f.name == "cppnix-narinfo-v1")
        .ok_or("missing cppnix-narinfo-v1 format")?;
    let parsed = narinfo::parse(&text, &fmt)?;
    emit_narinfo_table(&parsed, path);
    Ok(())
}

fn emit_narinfo_table(rec: &ParsedNarInfo, path: &str) {
    let banner = format!(
        "{}  {}  {}",
        glyph_snowflake(),
        header("narinfo"),
        muted(path),
    );
    println!("{banner}");
    println!();

    let label_w = 14;
    let kv = |k: &str, v: &str| {
        println!(
            "  {}  {}",
            body(&format!("{:>label_w$}", k, label_w = label_w)),
            ident(v),
        );
    };
    let opt = |k: &str, v: Option<&str>| {
        let display = v.unwrap_or("(none)");
        let val = if v.is_some() { info(display) } else { muted(display) };
        println!(
            "  {}  {}",
            body(&format!("{:>label_w$}", k, label_w = label_w)),
            val,
        );
    };

    kv("StorePath", &rec.store_path);
    kv("URL", &rec.url);
    kv("Compression", &rec.compression);
    kv("NarHash", &rec.nar_hash);
    kv("NarSize", &format!("{} bytes", rec.nar_size));
    opt("FileHash", rec.file_hash.as_deref());
    opt(
        "FileSize",
        rec.file_size
            .map(|n| format!("{n} bytes"))
            .as_deref(),
    );
    opt("Deriver", rec.deriver.as_deref());
    opt("System", rec.system.as_deref());
    opt("CA", rec.ca.as_deref());

    println!();
    println!(
        "  {}  {}",
        body(&format!("{:>label_w$}", "References", label_w = label_w)),
        ident(&rec.references.len().to_string()),
    );
    for r in &rec.references {
        println!("    {}  {}", muted("→"), success(r));
    }

    println!();
    println!(
        "  {}  {}",
        body(&format!("{:>label_w$}", "Signatures", label_w = label_w)),
        ident(&rec.signatures.len().to_string()),
    );
    for sig in &rec.signatures {
        // Format `<key>:<base64>` — colour the key.
        match sig.split_once(':') {
            Some((key, val)) => println!(
                "    {}  {}{}{}",
                muted("⎷"),
                info(key),
                muted(":"),
                muted(&truncate(val, 32)),
            ),
            None => println!("    {}  {}", muted("⎷"), muted(sig)),
        }
    }
}

// ── --registry-resolve mode ───────────────────────────────────────

fn emit_registry_resolve(flake_ref: &str) -> Result<(), Box<dyn std::error::Error>> {
    // The canonical registry formats only declare SHAPE (scope +
    // precedence + path).  Per-machine entries live in JSON files
    // on disk and aren't yet loaded by the substrate.  For now we
    // surface the algorithm via a deterministic demonstration set;
    // when sui-spec gains `registry::load_entries_from_disk`, this
    // mode lifts off it directly (third-site pattern: lock_file,
    // narinfo, soon registry).
    let formats = registry::load_canonical()?;
    let mut registries: registry::Registries = demonstration_entries(flake_ref);

    emit_registry_table(flake_ref, &registries, &formats);
    // Read clippy: registries is moved into the function — re-use
    // is fine here since it's consumed at the call site.
    let _ = &mut registries;
    Ok(())
}

/// Built-in demo entries so the precedence walk is visible to the
/// operator even before `registry::load_entries_from_disk` exists.
/// Walks the same algorithm `sui_spec::registry::resolve` will use
/// once the disk-loader lands.
fn demonstration_entries(flake_ref: &str) -> registry::Registries {
    vec![
        (
            RegistryScope::FlakeLocal,
            if flake_ref == "self" || flake_ref == "nixpkgs-overlay" {
                vec![RegistryEntry {
                    from: flake_ref.into(),
                    to: "github:pleme-io/substrate".into(),
                    exact: true,
                }]
            } else {
                vec![]
            },
        ),
        (
            RegistryScope::User,
            if flake_ref == "nixpkgs" {
                vec![RegistryEntry {
                    from: "nixpkgs".into(),
                    to: "github:NixOS/nixpkgs/nixos-unstable".into(),
                    exact: false,
                }]
            } else {
                vec![]
            },
        ),
        (
            RegistryScope::System,
            vec![],
        ),
        (
            RegistryScope::Global,
            match flake_ref {
                "nixpkgs" => vec![RegistryEntry {
                    from: "nixpkgs".into(),
                    to: "github:NixOS/nixpkgs".into(),
                    exact: false,
                }],
                "home-manager" => vec![RegistryEntry {
                    from: "home-manager".into(),
                    to: "github:nix-community/home-manager".into(),
                    exact: false,
                }],
                "flake-utils" => vec![RegistryEntry {
                    from: "flake-utils".into(),
                    to: "github:numtide/flake-utils".into(),
                    exact: false,
                }],
                _ => vec![],
            },
        ),
    ]
}

fn emit_registry_table(
    flake_ref: &str,
    registries: &registry::Registries,
    formats: &[registry::RegistryFormat],
) {
    let _ = formats; // Kept for upcoming default_path display.
    let banner = format!(
        "{}  {}  {}",
        glyph_snowflake(),
        header("registry resolve"),
        ident(flake_ref),
    );
    println!("{banner}");
    println!();

    // Sort scopes by precedence (flake-local first, global last).
    let mut sorted: Vec<&(RegistryScope, Vec<RegistryEntry>)> = registries.iter().collect();
    sorted.sort_by_key(|(scope, _)| scope_precedence(*scope));

    // Find the winning scope per cppnix precedence (lowest first).
    let mut winning_scope: Option<RegistryScope> = None;
    let mut winning_entry: Option<&RegistryEntry> = None;
    for (scope, entries) in &sorted {
        for entry in entries.iter() {
            if entry.from == flake_ref {
                winning_scope = Some(*scope);
                winning_entry = Some(entry);
                break;
            }
        }
        if winning_entry.is_some() {
            break;
        }
    }

    let scope_w = 14;
    println!(
        "  {}  {}",
        body(&format!("{:<scope_w$}", "Scope", scope_w = scope_w)),
        body("Entry"),
    );
    println!("  {}", muted(&"─".repeat(60)));
    for (scope, entries) in &sorted {
        let name = scope_name(*scope);
        let match_entry = entries.iter().find(|e| e.from == flake_ref);
        match match_entry {
            Some(entry) => {
                let wins = Some(*scope) == winning_scope;
                let marker = if wins { glyph_ok() } else { "·".to_string() };
                println!(
                    "  {} {}  {} {} {}{}",
                    marker,
                    success(&format!("{:<scope_w$}", name, scope_w = scope_w - 2)),
                    info(&entry.from),
                    muted("→"),
                    ident(&entry.to),
                    if entry.exact { format!("  {}", muted("[exact]")) } else { String::new() },
                );
            }
            None => {
                println!(
                    "  {} {}  {}",
                    muted("·"),
                    muted(&format!("{:<scope_w$}", name, scope_w = scope_w - 2)),
                    muted("(no entry)"),
                );
            }
        }
    }

    println!();
    match (winning_scope, winning_entry) {
        (Some(scope), Some(entry)) => {
            println!(
                "  {} resolves to {} via {}",
                glyph_arrow(),
                success(&entry.to),
                ident(scope_name(scope)),
            );
        }
        _ => {
            println!(
                "  {} {} {} {}",
                error("✘"),
                muted("no scope maps"),
                ident(flake_ref),
                muted("— would error: registry-unresolved"),
            );
        }
    }
}

fn scope_precedence(scope: RegistryScope) -> u32 {
    match scope {
        RegistryScope::FlakeLocal => 0,
        RegistryScope::User       => 1,
        RegistryScope::System     => 2,
        RegistryScope::Global     => 3,
    }
}

fn scope_name(scope: RegistryScope) -> &'static str {
    match scope {
        RegistryScope::FlakeLocal => "flake-local",
        RegistryScope::User       => "user",
        RegistryScope::System     => "system",
        RegistryScope::Global     => "global",
    }
}
