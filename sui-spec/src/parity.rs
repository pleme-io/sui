//! The [`ParityCheck`] trait — every typed parity question rides on it.
//!
//! Two sites today: [`crate::probe::Probe`] (eval-only, single-expression
//! probes) and [`crate::rebuild::RebuildProbe`] (host-aware, multi-stage
//! rebuild probes).  A third — `BuiltinSmoke` — is authored as a
//! [`Probe`] with a `"builtin-smoke"` tag, so it reuses the same impl.
//!
//! The trait factors out the four invariants every parity check obeys:
//!
//! 1. **identity** — a stable name + tag set the report can group by;
//! 2. **applicability** — whether the check runs in a given context
//!    (skip Darwin-only probes on Linux without recording a failure);
//! 3. **invocation** — typed [`Command`] construction for sui + nix
//!    (NO SHELL strings ever leave this layer);
//! 4. **classification** — the verdict given the two captured outputs.
//!
//! Sweep loops, report writers, and operator-facing wrappers are all
//! generic over `ParityCheck`, so a new typed domain that wants to
//! participate plugs in by implementing the trait once.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::exec::CapturedOutput;

/// Verdict for one (probe, context) combo.  The variant ordering
/// matches the priority of attention in the sweep summary: a `Differ`
/// is worse than a `BothFail`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Ord, PartialOrd)]
pub enum Verdict {
    /// Both engines succeeded and the comparison rule passed.
    Match,
    /// Probe declared itself non-applicable to this context (e.g. a
    /// Darwin-only rebuild stage on a Linux host).  Counts as a pass
    /// for summary purposes.
    NotApplicable,
    /// Both engines succeeded, but the comparison rule rejected.
    Differ,
    /// Only sui failed.  Most common failure mode while sui catches up.
    SuiFailOnly,
    /// Only nix (cppnix) failed.  Either sui caught a real bug nix
    /// papers over, or — more often — the probe expression is wrong.
    NixFailOnly,
    /// Both failed.  Probe is malformed or the flake itself is broken.
    BothFail,
    /// Sui hit the watchdog.
    SuiTimeout,
    /// Nix hit the watchdog.
    NixTimeout,
}

impl Verdict {
    /// Single-character glyph for the per-probe progress line.
    #[must_use]
    pub fn glyph(self) -> char {
        match self {
            Verdict::Match         => '.',
            Verdict::NotApplicable => 'a',
            Verdict::Differ        => 'D',
            Verdict::SuiFailOnly   => 'S',
            Verdict::NixFailOnly   => 'N',
            Verdict::BothFail      => '?',
            Verdict::SuiTimeout    => 's',
            Verdict::NixTimeout    => 'n',
        }
    }

    /// Nord-styled glyph for the per-probe progress line.  Used by
    /// `sui-sweep` (and any future operator-facing sweep surface)
    /// to color-code the verdict tide at a glance: green dots for
    /// matches, red glyphs for divergence, yellow for timeouts.
    #[must_use]
    pub fn glyph_styled(self) -> String {
        let g = self.glyph().to_string();
        match self {
            Verdict::Match         => crate::style::success(&g),
            Verdict::NotApplicable => crate::style::muted(&g),
            Verdict::Differ        => crate::style::error(&g),
            Verdict::SuiFailOnly   => crate::style::error(&g),
            Verdict::NixFailOnly   => crate::style::warn(&g),
            Verdict::BothFail      => crate::style::warn(&g),
            Verdict::SuiTimeout    => crate::style::pending(&g),
            Verdict::NixTimeout    => crate::style::pending(&g),
        }
    }

    /// `true` iff the verdict counts as a pass — `Match` or
    /// `NotApplicable`.  Used by the top-level summary line.
    #[must_use]
    pub fn is_pass(self) -> bool {
        matches!(self, Verdict::Match | Verdict::NotApplicable)
    }

    /// Stable string name, suitable for JSON keys and grouping.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Match         => "Match",
            Verdict::NotApplicable => "NotApplicable",
            Verdict::Differ        => "Differ",
            Verdict::SuiFailOnly   => "SuiFailOnly",
            Verdict::NixFailOnly   => "NixFailOnly",
            Verdict::BothFail      => "BothFail",
            Verdict::SuiTimeout    => "SuiTimeout",
            Verdict::NixTimeout    => "NixTimeout",
        }
    }
}

/// Classifies which corpus a probe came from.  Embedded in the report
/// so operators can filter without re-parsing the original Lisp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeKind {
    /// `(defprobe ...)` — a single Nix expression with `$FLAKE`
    /// substitution.
    Eval,
    /// `(defrebuild-probe ...)` — host-aware multi-stage rebuild
    /// invocation.
    Rebuild,
    /// `(defprobe ...)` with the `builtin-smoke` tag — exercises one
    /// of sui's builtin modules.
    BuiltinSmoke,
}

impl ProbeKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeKind::Eval         => "eval",
            ProbeKind::Rebuild      => "rebuild",
            ProbeKind::BuiltinSmoke => "builtin-smoke",
        }
    }
}

/// Operator host platform — fixes the OS the probe sweep is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetOs {
    Darwin,
    Linux,
    Other,
}

impl TargetOs {
    /// Read from `std::env::consts::OS`.
    #[must_use]
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos"  => TargetOs::Darwin,
            "linux"  => TargetOs::Linux,
            _        => TargetOs::Other,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TargetOs::Darwin => "darwin",
            TargetOs::Linux  => "linux",
            TargetOs::Other  => "other",
        }
    }
}

/// Operator-machine architecture (`aarch64-darwin`, `x86_64-linux`, ...).
///
/// Derived from `std::env::consts::ARCH` + [`TargetOs`].  Used to pick
/// the right `packages.<system>` / `devShells.<system>` attribute under
/// rebuild probes.
#[must_use]
pub fn current_nix_system() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64"      => "x86_64",
        "aarch64"     => "aarch64",
        other          => other,
    };
    let os = match TargetOs::current() {
        TargetOs::Darwin => "darwin",
        TargetOs::Linux  => "linux",
        TargetOs::Other  => std::env::consts::OS,
    };
    format!("{arch}-{os}")
}

/// Operator hostname (short form — `hostname -s` semantics).
#[must_use]
pub fn current_hostname() -> String {
    // Read /etc/hostname first (Linux + nix-darwin both populate it),
    // then fall back to the `HOSTNAME` env, then to "unknown".
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            // Strip the FQDN suffix to match `hostname -s`.
            return trimmed.split('.').next().unwrap_or(trimmed).to_string();
        }
    }
    if let Ok(s) = std::env::var("HOSTNAME") {
        if !s.is_empty() {
            return s.split('.').next().unwrap_or(&s).to_string();
        }
    }
    // Last resort: spawn `hostname -s`.  This file should never be hot,
    // so the subprocess cost is fine.
    if let Ok(out) = std::process::Command::new("hostname").arg("-s").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "unknown".to_string()
}

/// Operator-machine context every probe receives during a sweep.
///
/// All fields are populated from the current machine; the sweep doesn't
/// (yet) iterate hosts independently of the operator running it.
#[derive(Debug, Clone)]
pub struct ProbeContext {
    /// Absolute path to the flake being probed (the directory
    /// containing `flake.nix`).
    pub flake_path: PathBuf,
    /// Short label for reports — typically the flake directory's
    /// basename.
    pub flake_label: String,
    /// Operator hostname.
    pub host: String,
    /// Nix system tuple — `aarch64-darwin`, `x86_64-linux`, ...
    pub system: String,
    /// Operator username (`$USER` or current uid lookup).
    pub user: String,
    /// Operator OS.
    pub os: TargetOs,
}

impl ProbeContext {
    /// Build a context for the current operator + `flake_path`.
    #[must_use]
    pub fn current(flake_path: PathBuf) -> Self {
        let flake_label = flake_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| flake_path.display().to_string());
        Self {
            flake_path,
            flake_label,
            host: current_hostname(),
            system: current_nix_system(),
            user: std::env::var("USER").unwrap_or_else(|_| "unknown".into()),
            os: TargetOs::current(),
        }
    }

    /// Substitute `$FLAKE` / `$HOST` / `$SYSTEM` / `$USER` placeholders
    /// in `template`.  Order-insensitive; the placeholder set is
    /// closed (no recursive expansion).
    #[must_use]
    pub fn substitute(&self, template: &str) -> String {
        template
            .replace("$FLAKE", &self.flake_path.display().to_string())
            .replace("$HOST", &self.host)
            .replace("$SYSTEM", &self.system)
            .replace("$USER", &self.user)
    }
}

/// Every typed parity domain implements this trait.  Sweep loops drive
/// `&dyn ParityCheck`; corpora produce `Vec<Box<dyn ParityCheck>>` via
/// trait-object boxing in the corpus loader.
pub trait ParityCheck {
    /// Stable probe name (must be unique within a corpus).
    fn name(&self) -> &str;

    /// Tags attached to this probe.  Sweep filters include/exclude by
    /// tag membership.
    fn tags(&self) -> &[String];

    /// Which corpus this probe belongs to.  Embedded in the report.
    fn kind(&self) -> ProbeKind;

    /// Whether this probe applies to the given context.  Default is
    /// always-applies; rebuild probes override to skip e.g. Darwin
    /// stages on Linux.
    fn applies(&self, _ctx: &ProbeContext) -> bool {
        true
    }

    /// Construct the `sui` invocation for this probe.  Implementations
    /// must NOT use shell strings — every argument is added via typed
    /// [`Command`] APIs.
    fn sui_invocation(&self, ctx: &ProbeContext, sui_bin: &Path) -> Command;

    /// Construct the `nix` invocation for this probe (the cppnix oracle).
    fn nix_invocation(&self, ctx: &ProbeContext, nix_bin: &Path) -> Command;

    /// Classify the (sui, nix) output pair.  Default behavior covers
    /// the spawn/timeout/exit-code matrix; overrides handle the
    /// comparison-rule layer.
    fn classify(&self, sui: &CapturedOutput, nix: &CapturedOutput) -> Verdict {
        default_classify(sui, nix, |s, n| s.stdout.trim() == n.stdout.trim())
    }
}

/// Shared verdict skeleton — handles spawn failure, timeout, and the
/// exit-code matrix.  Comparison-rule logic plugs in via the
/// `compare_ok` closure, which is only called when both engines exit 0.
pub fn default_classify(
    sui: &CapturedOutput,
    nix: &CapturedOutput,
    compare_ok: impl FnOnce(&CapturedOutput, &CapturedOutput) -> bool,
) -> Verdict {
    match (sui.timed_out, nix.timed_out) {
        (true, _)  => return Verdict::SuiTimeout,
        (_, true)  => return Verdict::NixTimeout,
        (false, false) => {}
    }
    match (sui.success, nix.success) {
        (true, true) => if compare_ok(sui, nix) { Verdict::Match } else { Verdict::Differ },
        (false, true) => Verdict::SuiFailOnly,
        (true, false) => Verdict::NixFailOnly,
        (false, false) => Verdict::BothFail,
    }
}

// ── Report types ────────────────────────────────────────────────────

/// Top-level shadow-sweep report.  Serialised as JSON to
/// `~/.cache/sui/shadow-reports/<host>-<ISO-8601>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowReport {
    /// ISO-8601 UTC timestamp.
    pub generated_at: String,
    /// Tool that produced the report (`"sui-sweep <version>"`).
    pub generator: String,
    /// Operator hostname.
    pub host: String,
    /// Operator nix system tuple.
    pub system: String,
    /// Operator OS short name.
    pub os: String,
    /// Operator username.
    pub user: String,
    /// `sui --version` stdout if available.
    pub sui_version: Option<String>,
    /// `nix --version` stdout if available.
    pub nix_version: Option<String>,
    /// Per-(probe, flake) record.
    pub records: Vec<ProbeRecord>,
    /// Verdict-name → count tally.
    pub tally: BTreeMap<String, usize>,
}

/// The value-witness of a [`SweepVerdict`] — the record set the verdict
/// was derived from, plus the worst [`Verdict`] in it.
///
/// UNREPRESENTABILITY §II.4 clause 2: the pass arm must be
/// non-constructible without a non-empty subject-set witness.  That is
/// structural here — [`Examined::from_records`] is **private** and
/// returns `Option`, so the only way to obtain an `Examined` is to hand
/// it a non-empty `&[ProbeRecord]`, and the only code that can do so
/// lives in this module.  No public constructor exists, so no caller
/// can name a pass over subjects it never examined.
///
/// `worst` is where the 8-way [`Verdict`] ordering earns its keep: the
/// enum's `Ord` derive documents "a `Differ` is worse than a
/// `BothFail`", and until now nothing in sui consumed it.  `max()` over
/// the records is that ranking, and because `Iterator::max` yields
/// `None` on an empty iterator, **the emptiness check and the severity
/// ranking are the same operation** — there is no separate `is_empty()`
/// branch to forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Examined {
    count: NonZeroUsize,
    worst: Verdict,
}

impl Examined {
    /// The sole ingress.  Private by design (see the type doc).
    fn from_records(records: &[ProbeRecord]) -> Option<Self> {
        // `max()` returns None exactly when `records` is empty, so this
        // single call is both the non-emptiness proof and the severity
        // ranking over the 8-way `Verdict` `Ord`.
        let worst = records.iter().map(|r| r.verdict).max()?;
        let count = NonZeroUsize::new(records.len())?;
        Some(Self { count, worst })
    }

    /// How many records the verdict was derived from.  A projection of
    /// the value — never a field a caller sets.
    #[must_use]
    pub fn count(&self) -> NonZeroUsize {
        self.count
    }

    /// The worst verdict among the examined records, by the 8-way
    /// [`Verdict`] ordering.
    #[must_use]
    pub fn worst(&self) -> Verdict {
        self.worst
    }
}

/// The verdict of one shadow sweep — derived from the sweep's records,
/// never asserted.
///
/// UNREPRESENTABILITY §II.4.  `AllPassed` cannot be named without an
/// [`Examined`] witness, and "we compared nothing" is [`Self::Vacuous`]
/// — a **distinct arm**, not a pass.
///
/// **What emptiness means here, decided once and visibly** (§II.4 does
/// not say "empty fails"; it says emptiness must be *sayable*, and the
/// domain decides).  sui's entire product is a byte-parity proof against
/// nix, and this repo's own INSTRUMENT RULE records the cost of getting
/// this wrong: a `SUI_PARITY_PUREONLY=1` default once shipped "a total
/// loss of nixpkgs evaluation as a green check".  A sweep that compared
/// nothing has proven nothing, so `Vacuous` is **not** a pass and the
/// CLI exits non-zero on it.  (This is skill-lint's decision — "a linter
/// that validated nothing validated nothing" — not magma's
/// `Coverage::Vacuous`, which supports an in-sync claim because an empty
/// world cannot drift.  Both are correct in their own domain; the point
/// of the arm is that the choice is stated rather than defaulted.)
///
/// Note there is deliberately **no stored verdict field on
/// [`ShadowReport`]**, and therefore no serde border to forge across:
/// the verdict is re-derived from the serialized `records` on every
/// call, so a hand-edited report cannot claim a pass its records do not
/// support.  Adding a persisted `verdict` field would *create* the
/// forgery surface §II.4's fourth property then has to defend against.
///
/// `#[non_exhaustive]` is free here — the enum is new, so it has no
/// existing downstream matches to break — and it converts every future
/// arm from a downstream migration into a non-event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SweepVerdict {
    /// Every examined record passed.  Carries the witness.
    AllPassed { examined: Examined },
    /// At least one examined record diverged.
    Diverged {
        examined: Examined,
        diverged: NonZeroUsize,
    },
    /// The sweep ran but examined zero records — no claim is made.
    ///
    /// Reachable in normal operation: a `--tag` filter that matches no
    /// probe, or a `flakes_root` that does not exist or holds no
    /// `flake.nix` (`sweep::resolve_flakes` returns an empty `Vec`
    /// rather than erroring).  Before this arm existed, every one of
    /// those paths printed a green ✓ and exited 0.
    Vacuous,
}

impl SweepVerdict {
    /// `true` only for [`Self::AllPassed`].  Total over the closed sum —
    /// there is no `_ =>` arm that could round `Vacuous` up.
    #[must_use]
    pub const fn is_pass(&self) -> bool {
        matches!(self, Self::AllPassed { .. })
    }

    /// The witness, when the verdict has one.
    #[must_use]
    pub const fn examined(&self) -> Option<Examined> {
        match self {
            Self::AllPassed { examined } | Self::Diverged { examined, .. } => Some(*examined),
            Self::Vacuous => None,
        }
    }
}

impl ShadowReport {
    /// The ONE classifier — a total function of `self.records`.
    ///
    /// No constructor anywhere accepts a [`SweepVerdict`]; this is the
    /// only way to obtain one (§II.4 clause 1).
    #[must_use]
    pub fn verdict(&self) -> SweepVerdict {
        let Some(examined) = Examined::from_records(&self.records) else {
            return SweepVerdict::Vacuous;
        };
        match NonZeroUsize::new(self.divergence_count()) {
            Some(diverged) => SweepVerdict::Diverged { examined, diverged },
            None => SweepVerdict::AllPassed { examined },
        }
    }

    /// `true` iff the sweep examined at least one record and every one
    /// of them passed.
    ///
    /// Derived from [`Self::verdict`], so the vacuous case can no longer
    /// reach this `true`.  Prefer matching on [`Self::verdict`] directly
    /// where the operator needs to know *why* a sweep is not a pass —
    /// this bool cannot distinguish `Vacuous` from `Diverged`.
    #[must_use]
    pub fn all_pass(&self) -> bool {
        self.verdict().is_pass()
    }

    /// Count of non-passing records.
    ///
    /// A raw count, not a verdict: `0` here means "no divergence among
    /// the records present", which over an empty record set says nothing
    /// at all.  Gate on [`Self::verdict`], never on `divergence_count()
    /// == 0` — that comparison is the same vacuous-pass defect wearing
    /// an integer.
    #[must_use]
    pub fn divergence_count(&self) -> usize {
        self.records.iter().filter(|r| !r.verdict.is_pass()).count()
    }
}

/// One probe × one flake = one record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeRecord {
    pub name: String,
    pub kind: ProbeKind,
    pub tags: Vec<String>,
    pub flake: String,
    pub sui_argv: Vec<String>,
    pub nix_argv: Vec<String>,
    pub sui_exit: Option<i32>,
    pub nix_exit: Option<i32>,
    pub sui_stdout_excerpt: String,
    pub nix_stdout_excerpt: String,
    pub sui_stderr_excerpt: String,
    pub nix_stderr_excerpt: String,
    pub sui_duration_ms: u128,
    pub nix_duration_ms: u128,
    pub sui_timed_out: bool,
    pub nix_timed_out: bool,
    pub verdict: Verdict,
}

/// Truncate a string to `max` bytes, appending an ellipsis if cut.
/// UTF-8-safe — cuts at the previous char boundary.
#[must_use]
pub fn excerpt(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_pass_set() {
        assert!(Verdict::Match.is_pass());
        assert!(Verdict::NotApplicable.is_pass());
        assert!(!Verdict::Differ.is_pass());
        assert!(!Verdict::SuiFailOnly.is_pass());
        assert!(!Verdict::SuiTimeout.is_pass());
    }

    #[test]
    fn substitute_replaces_all_placeholders() {
        let ctx = ProbeContext {
            flake_path: PathBuf::from("/tmp/myflake"),
            flake_label: "myflake".into(),
            host: "cid".into(),
            system: "aarch64-darwin".into(),
            user: "drzzln".into(),
            os: TargetOs::Darwin,
        };
        let out = ctx.substitute("path:$FLAKE host=$HOST sys=$SYSTEM user=$USER");
        assert_eq!(out, "path:/tmp/myflake host=cid sys=aarch64-darwin user=drzzln");
    }

    #[test]
    fn excerpt_truncates_at_char_boundary() {
        let s = "ab".repeat(100);
        let e = excerpt(&s, 20);
        assert!(e.ends_with('…'));
        assert!(e.chars().count() <= 25);
    }

    #[test]
    fn default_classify_handles_full_matrix() {
        let ok = mk_out(true, false);
        let fail = mk_out(false, false);
        let timeout = mk_out(false, true);

        let always_eq = |_s: &CapturedOutput, _n: &CapturedOutput| true;
        let always_neq = |_s: &CapturedOutput, _n: &CapturedOutput| false;

        assert_eq!(default_classify(&ok, &ok, always_eq), Verdict::Match);
        assert_eq!(default_classify(&ok, &ok, always_neq), Verdict::Differ);
        assert_eq!(default_classify(&fail, &ok, always_eq), Verdict::SuiFailOnly);
        assert_eq!(default_classify(&ok, &fail, always_eq), Verdict::NixFailOnly);
        assert_eq!(default_classify(&fail, &fail, always_eq), Verdict::BothFail);
        assert_eq!(default_classify(&timeout, &ok, always_eq), Verdict::SuiTimeout);
        assert_eq!(default_classify(&ok, &timeout, always_eq), Verdict::NixTimeout);
    }

    fn mk_out(success: bool, timed_out: bool) -> CapturedOutput {
        CapturedOutput {
            exit_code: if success { Some(0) } else { Some(1) },
            success,
            stdout: String::new(),
            stderr: String::new(),
            duration: std::time::Duration::from_millis(1),
            timed_out,
        }
    }

    /// The inverted red-run.  Against unfixed code (85ddfec) this
    /// assertion's opposite held: `all_pass()` returned `true` over zero
    /// records, so a parity sweep that compared nothing certified
    /// parity.  UNREPRESENTABILITY §II.4.
    #[test]
    fn vacuous_sweep_is_not_a_pass() {
        let report = empty_report();
        assert_eq!(report.verdict(), SweepVerdict::Vacuous);
        assert!(
            !report.all_pass(),
            "a sweep that ran ZERO probes must not report all-pass",
        );
        // The count is still honestly 0 — it is the *verdict* that must
        // not round that up.
        assert_eq!(report.divergence_count(), 0);
    }

    /// Clause 2, mechanically: the pass arm carries its witness, and the
    /// witness is a projection of the records, not a settable field.
    #[test]
    fn pass_arm_carries_a_non_empty_witness() {
        let report = report_with(&[Verdict::Match, Verdict::NotApplicable]);
        match report.verdict() {
            SweepVerdict::AllPassed { examined } => {
                assert_eq!(examined.count().get(), 2);
                // NotApplicable > Match in the 8-way ordering.
                assert_eq!(examined.worst(), Verdict::NotApplicable);
            }
            other => panic!("expected AllPassed, got {other:?}"),
        }
    }

    /// The 8-way `Verdict` ordering survives — and is now load-bearing.
    /// `Examined::worst` is `max()` over that `Ord`, so the deliberate
    /// "a `Differ` is worse than a `BothFail`" ranking is consumed
    /// rather than merely derived.
    #[test]
    fn witness_worst_uses_the_eight_way_ordering() {
        // Ordering as declared on the enum.
        assert!(Verdict::Differ < Verdict::BothFail);
        assert!(Verdict::Match < Verdict::Differ);

        let report = report_with(&[Verdict::Match, Verdict::BothFail, Verdict::Differ]);
        match report.verdict() {
            SweepVerdict::Diverged { examined, diverged } => {
                assert_eq!(diverged.get(), 2);
                assert_eq!(examined.count().get(), 3);
                assert_eq!(examined.worst(), Verdict::BothFail);
            }
            other => panic!("expected Diverged, got {other:?}"),
        }
    }

    /// A `Vacuous` verdict has no witness to hand out, and cannot be
    /// coaxed into reporting a pass.
    #[test]
    fn vacuous_has_no_witness() {
        assert!(SweepVerdict::Vacuous.examined().is_none());
        assert!(!SweepVerdict::Vacuous.is_pass());
    }

    /// The verdict is re-derived from the serialized records, so a
    /// round-tripped report cannot carry a forged pass — there is no
    /// stored verdict field to forge.
    #[test]
    fn verdict_is_rederived_across_the_serde_border() {
        let report = report_with(&[Verdict::Match, Verdict::Differ]);
        let json = serde_json::to_string(&report).expect("serialize");
        let back: ShadowReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.verdict(), report.verdict());
        assert!(!back.all_pass());

        let empty_json = serde_json::to_string(&empty_report()).expect("serialize");
        let back_empty: ShadowReport = serde_json::from_str(&empty_json).expect("deserialize");
        assert_eq!(back_empty.verdict(), SweepVerdict::Vacuous);
    }

    fn report_with(verdicts: &[Verdict]) -> ShadowReport {
        let mut report = empty_report();
        report.records = verdicts
            .iter()
            .map(|v| ProbeRecord {
                name: "p".into(),
                kind: ProbeKind::Eval,
                tags: vec![],
                flake: "f".into(),
                sui_argv: vec![],
                nix_argv: vec![],
                sui_exit: Some(0),
                nix_exit: Some(0),
                sui_stdout_excerpt: String::new(),
                nix_stdout_excerpt: String::new(),
                sui_stderr_excerpt: String::new(),
                nix_stderr_excerpt: String::new(),
                sui_duration_ms: 0,
                nix_duration_ms: 0,
                sui_timed_out: false,
                nix_timed_out: false,
                verdict: *v,
            })
            .collect();
        report
    }

    fn empty_report() -> ShadowReport {
        ShadowReport {
            generated_at: "2026-07-28T00:00:00Z".into(),
            generator: "sui-sweep test".into(),
            host: "cid".into(),
            system: "aarch64-darwin".into(),
            os: "darwin".into(),
            user: "drzzln".into(),
            sui_version: None,
            nix_version: None,
            records: Vec::new(),
            tally: BTreeMap::new(),
        }
    }

    #[test]
    fn shadow_report_pass_counts_records() {
        let rec_pass = ProbeRecord {
            name: "p".into(), kind: ProbeKind::Eval, tags: vec![],
            flake: "f".into(), sui_argv: vec![], nix_argv: vec![],
            sui_exit: Some(0), nix_exit: Some(0),
            sui_stdout_excerpt: String::new(), nix_stdout_excerpt: String::new(),
            sui_stderr_excerpt: String::new(), nix_stderr_excerpt: String::new(),
            sui_duration_ms: 0, nix_duration_ms: 0,
            sui_timed_out: false, nix_timed_out: false,
            verdict: Verdict::Match,
        };
        let mut rec_fail = rec_pass.clone();
        rec_fail.verdict = Verdict::Differ;
        let report = ShadowReport {
            generated_at: "2026-05-22T00:00:00Z".into(),
            generator: "sui-sweep 0.1".into(),
            host: "cid".into(),
            system: "aarch64-darwin".into(),
            os: "darwin".into(),
            user: "drzzln".into(),
            sui_version: None, nix_version: None,
            records: vec![rec_pass.clone(), rec_fail, rec_pass],
            tally: BTreeMap::new(),
        };
        assert!(!report.all_pass());
        assert_eq!(report.divergence_count(), 1);
    }
}
