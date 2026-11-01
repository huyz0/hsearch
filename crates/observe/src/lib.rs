//! Foundational observation boundary. No telemetry exporter or diagnostic service exists yet.

use hsearch_types::Progress;

/// Observe a validated progress snapshot without receiving document contents or credentials.
///
/// Implementations must bound their own work and buffering. Callers must not derive
/// durability or successful storage operations from delivery of an observation.
pub trait ProgressObserver: Send + Sync {
    /// Record a snapshot using the observer's explicitly documented loss policy.
    fn record(&self, progress: Progress);
}
