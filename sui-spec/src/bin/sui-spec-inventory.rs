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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse()?;
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
    println!("┌─────────────────┬──────┐");
    println!("│ Gate            │ Count│");
    println!("├─────────────────┼──────┤");
    for (gate, count) in &hist {
        println!("│ {:<15} │ {:>4} │", gate, count);
    }
    println!("├─────────────────┼──────┤");
    println!("│ Total           │ {:>4} │", cat.len());
    println!("└─────────────────┴──────┘");
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

    println!(
        "{:<name_w$}  {:<gate_w$}  {:<kw_w$}  Purpose",
        "Domain", "Gate", "Keyword(s)",
        name_w = name_w, gate_w = gate_w, kw_w = kw_w,
    );
    println!("{}", "─".repeat(name_w + gate_w + kw_w + 30));
    for d in domains {
        let kws = d.authoring_keywords.join(", ");
        let kw_trunc = if kws.len() > kw_w {
            format!("{}…", &kws[..kw_w.saturating_sub(1)])
        } else {
            kws.clone()
        };
        println!(
            "{:<name_w$}  {:<gate_w$}  {:<kw_w$}  {}",
            d.name, gate_name(d.gate), kw_trunc, d.purpose,
            name_w = name_w, gate_w = gate_w, kw_w = kw_w,
        );
    }
}
