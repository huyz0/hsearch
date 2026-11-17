//! Which AES-GCM implementation this process runs, reported rather than assumed.
//!
//! The cipher is the vetted `aes` and `ghash` crates; this module does not implement any cryptography. Those
//! crates pick an implementation at run time (AES-NI or VAES on x86, the ARMv8 crypto extension on aarch64,
//! portable software everywhere else) and fall back to software when the CPU lacks the instructions, so an
//! unsupported instruction is never executed. Two build switches force the portable path for every CPU:
//! `--cfg aes_backend="soft"` and `--cfg polyval_backend="soft"` (see `tests/backend.rs` and the CI step that
//! sets both). All paths compute the same bytes and verify the same tag; nothing here can turn verification
//! off, and there is no plaintext path to fall back to.
//!
//! [`report`] states what is in use. The AES answer is the `aes` crate's own `hardware_accelerated()`; the
//! GHASH answer repeats the feature test the `ghash` backend uses (its dispatch is not exposed), so it is
//! evidence for operators and tests, not a second source of truth for the cipher.

use std::fmt;

/// An implementation family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Impl {
    /// Portable software.
    Soft,
    /// CPU instructions (AES-NI/VAES, ARMv8 AES; PCLMULQDQ, PMULL).
    Hardware,
}

impl Impl {
    /// A stable label for metrics and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Impl::Soft => "soft",
            Impl::Hardware => "hardware",
        }
    }
}

/// The CPU features that matter to the AEAD, as the process sees them. Features the architecture does not
/// have are `false`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuFeatures {
    /// x86 AES-NI or aarch64 AES.
    pub aes: bool,
    /// x86 PCLMULQDQ or aarch64 PMULL (which is part of the aarch64 `aes` feature).
    pub carryless_multiply: bool,
    /// x86 VAES (a wider AES path; optional, reported only).
    pub vaes: bool,
}

/// What this process runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendReport {
    /// `x86_64`, `x86`, `aarch64` or `other`.
    pub arch: &'static str,
    /// The block cipher.
    pub aes: Impl,
    /// The authenticator (GHASH).
    pub ghash: Impl,
    /// Whether the build forces the software AES and GHASH paths.
    pub forced_soft: bool,
    /// What the CPU offers.
    pub cpu: CpuFeatures,
}

impl BackendReport {
    /// Whether either primitive runs in software.
    pub fn any_soft(&self) -> bool {
        self.aes == Impl::Soft || self.ghash == Impl::Soft
    }
}

impl fmt::Display for BackendReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "arch={} aes={} ghash={} forced_soft={} cpu_aes={} cpu_clmul={} cpu_vaes={}",
            self.arch,
            self.aes.as_str(),
            self.ghash.as_str(),
            self.forced_soft,
            self.cpu.aes,
            self.cpu.carryless_multiply,
            self.cpu.vaes
        )
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
fn detect() -> (&'static str, CpuFeatures, bool) {
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "x86"
    };
    let cpu = CpuFeatures {
        aes: std::arch::is_x86_feature_detected!("aes"),
        carryless_multiply: std::arch::is_x86_feature_detected!("pclmulqdq"),
        vaes: std::arch::is_x86_feature_detected!("vaes"),
    };
    // The ghash backend needs both AVX and PCLMULQDQ.
    let ghash_ok = cpu.carryless_multiply && std::arch::is_x86_feature_detected!("avx");
    (arch, cpu, ghash_ok)
}

#[cfg(target_arch = "aarch64")]
fn detect() -> (&'static str, CpuFeatures, bool) {
    let aes = std::arch::is_aarch64_feature_detected!("aes");
    let cpu = CpuFeatures {
        aes,
        carryless_multiply: std::arch::is_aarch64_feature_detected!("pmull"),
        vaes: false,
    };
    (
        "aarch64",
        cpu,
        std::arch::is_aarch64_feature_detected!("aes"),
    )
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")))]
fn detect() -> (&'static str, CpuFeatures, bool) {
    (
        "other",
        CpuFeatures {
            aes: false,
            carryless_multiply: false,
            vaes: false,
        },
        false,
    )
}

/// Whether the build forces software (both switches set).
#[allow(unexpected_cfgs)]
const fn forced_soft() -> bool {
    cfg!(aes_backend = "soft") && cfg!(polyval_backend = "soft")
}

#[allow(unexpected_cfgs)]
const fn ghash_forced_soft() -> bool {
    cfg!(polyval_backend = "soft")
}

/// What this process runs. Cheap (the detections are cached by the standard library and the `aes` crate).
pub fn report() -> BackendReport {
    let (arch, cpu, ghash_hw) = detect();
    BackendReport {
        arch,
        aes: if aes::hardware_accelerated() {
            Impl::Hardware
        } else {
            Impl::Soft
        },
        ghash: if ghash_hw && !ghash_forced_soft() {
            Impl::Hardware
        } else {
            Impl::Soft
        },
        forced_soft: forced_soft(),
        cpu,
    }
}
