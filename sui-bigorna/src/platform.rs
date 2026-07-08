//! Typed OCI platform + architecture values.
//!
//! A build platform (`linux/amd64`, `linux/arm64/v8`) is a typed value with a
//! closed OS + architecture set — never a free-form string flung at `docker
//! buildx build --platform`. The [`Display`] impls are the *only* place a
//! platform token is rendered (per the fleet's TYPED EMISSION rule: rendering
//! lives in a `Display` impl, never a `format!()` of syntax), and [`FromStr`]
//! is the single parse boundary that rejects anything outside the closed set.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The operating-system half of a platform. Closed set — an unknown OS token
/// is rejected at the [`FromStr`] boundary rather than carried downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    Linux,
    Windows,
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Os::Linux => "linux",
            Os::Windows => "windows",
        })
    }
}

impl FromStr for Os {
    type Err = PlatformParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "linux" => Ok(Os::Linux),
            "windows" => Ok(Os::Windows),
            other => Err(PlatformParseError::UnknownOs(other.to_string())),
        }
    }
}

/// The CPU architecture half of a platform (an OCI `GOARCH`). Closed set — the
/// architectures the fleet's native build nodes can actually target. An unknown
/// arch is rejected at parse time so an "arch we can only emulate" can never be
/// mistaken for a native one downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    Amd64,
    Arm64,
    Arm,
    I386,
    Ppc64le,
    S390x,
    Riscv64,
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
            Arch::Arm => "arm",
            Arch::I386 => "386",
            Arch::Ppc64le => "ppc64le",
            Arch::S390x => "s390x",
            Arch::Riscv64 => "riscv64",
        })
    }
}

impl FromStr for Arch {
    type Err = PlatformParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "amd64" | "x86_64" => Ok(Arch::Amd64),
            "arm64" | "aarch64" => Ok(Arch::Arm64),
            "arm" => Ok(Arch::Arm),
            "386" | "i386" | "i686" => Ok(Arch::I386),
            "ppc64le" => Ok(Arch::Ppc64le),
            "s390x" => Ok(Arch::S390x),
            "riscv64" => Ok(Arch::Riscv64),
            other => Err(PlatformParseError::UnknownArch(other.to_string())),
        }
    }
}

impl Arch {
    /// The architecture of the host this process is running on, derived from
    /// the compile-time target. Used to prove a node is *native* (host arch ==
    /// target arch) rather than an emulated fallback.
    #[must_use]
    pub fn host() -> Option<Self> {
        // `std::env::consts::ARCH` is the canonical host-arch token.
        Self::from_str(std::env::consts::ARCH).ok()
    }
}

// An architecture serializes to / from its canonical OCI token (`amd64`,
// `arm64`) so a YAML/JSON config carries `host_arch: arm64`, not `Arm64`.
impl Serialize for Arch {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Arch {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse::<Arch>().map_err(serde::de::Error::custom)
    }
}

/// A fully typed build platform: an OS, an architecture, and an optional
/// micro-arch variant (`v2`, `v7`, `v8`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
    pub variant: Option<String>,
}

impl Platform {
    /// Construct a variant-less platform.
    #[must_use]
    pub fn new(os: Os, arch: Arch) -> Self {
        Self { os, arch, variant: None }
    }

    /// `linux/amd64`.
    #[must_use]
    pub fn linux_amd64() -> Self {
        Self::new(Os::Linux, Arch::Amd64)
    }

    /// `linux/arm64`.
    #[must_use]
    pub fn linux_arm64() -> Self {
        Self::new(Os::Linux, Arch::Arm64)
    }

    /// Whether `self` and `other` are the same native target (same OS + arch),
    /// ignoring the micro-arch variant — a `linux/amd64` node natively serves a
    /// `linux/amd64/v2` request because it is the same physical CPU family.
    #[must_use]
    pub fn same_native_target(&self, other: &Platform) -> bool {
        self.os == other.os && self.arch == other.arch
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.arch)?;
        if let Some(variant) = &self.variant {
            write!(f, "/{variant}")?;
        }
        Ok(())
    }
}

impl FromStr for Platform {
    type Err = PlatformParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split('/');
        let os = parts
            .next()
            .ok_or_else(|| PlatformParseError::Malformed(s.to_string()))?
            .parse::<Os>()?;
        let arch = parts
            .next()
            .ok_or_else(|| PlatformParseError::Malformed(s.to_string()))?
            .parse::<Arch>()?;
        let variant = parts.next().map(str::to_string);
        Ok(Self { os, arch, variant })
    }
}

// A platform serializes to / from its canonical `os/arch[/variant]` string so
// a YAML/JSON config carries `platform: linux/arm64`, not a nested object.
impl Serialize for Platform {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Platform {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse::<Platform>().map_err(serde::de::Error::custom)
    }
}

/// An ordered list of platforms — the value behind a single `--platform
/// a,b,c` flag. Rendering the comma-join lives here (a `Display` impl), never
/// an ad-hoc string join at a call site.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlatformList(pub Vec<Platform>);

impl PlatformList {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Display for PlatformList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, p) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{p}")?;
        }
        Ok(())
    }
}

/// Errors from the single platform parse boundary.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlatformParseError {
    #[error("unknown OS token `{0}` (expected linux|windows)")]
    UnknownOs(String),
    #[error("unknown architecture token `{0}`")]
    UnknownArch(String),
    #[error("malformed platform `{0}` (expected os/arch[/variant])")]
    Malformed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_roundtrips_through_its_canonical_string() {
        for s in ["linux/amd64", "linux/arm64", "linux/arm64/v8", "linux/386"] {
            let p: Platform = s.parse().unwrap();
            assert_eq!(p.to_string(), s);
        }
    }

    #[test]
    fn unknown_arch_is_rejected_at_the_parse_boundary() {
        let err = "linux/sparc".parse::<Platform>().unwrap_err();
        assert_eq!(err, PlatformParseError::UnknownArch("sparc".to_string()));
    }

    #[test]
    fn arch_aliases_normalize() {
        assert_eq!("x86_64".parse::<Arch>().unwrap(), Arch::Amd64);
        assert_eq!("aarch64".parse::<Arch>().unwrap(), Arch::Arm64);
    }

    #[test]
    fn platform_list_renders_a_single_comma_joined_flag_value() {
        let list = PlatformList(vec![Platform::linux_amd64(), Platform::linux_arm64()]);
        assert_eq!(list.to_string(), "linux/amd64,linux/arm64");
    }

    #[test]
    fn same_native_target_ignores_variant() {
        let a: Platform = "linux/amd64".parse().unwrap();
        let b: Platform = "linux/amd64/v2".parse().unwrap();
        assert!(a.same_native_target(&b));
        let c: Platform = "linux/arm64".parse().unwrap();
        assert!(!a.same_native_target(&c));
    }

    #[test]
    fn platform_serde_is_a_plain_string() {
        let p = Platform::linux_arm64();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"linux/arm64\"");
        let back: Platform = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }
}
