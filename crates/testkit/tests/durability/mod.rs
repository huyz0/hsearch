//! Candidate-specific code is test-only and uses the libraries' actual protocols.
mod budget;
mod common;
mod openraft;
mod raft_rs;
mod sample_gate;
mod workload;

pub(super) fn raft_ready_persistence_probe() {
    raft_rs::ready_persistence_probe();
}
pub(super) fn openraft_persistence_probe() {
    openraft::persistence_probe();
}

pub(super) fn fixed_offered_admission_controls() {
    sample_gate::controls();
    workload::controls();
    budget::controls();
    raft_rs::codec_controls();
}

pub(super) fn matched_raft_workload_experiment() {
    let _admission = sample_gate::enter("raft-rs");
    raft_rs::matched_workload_experiment();
}
pub(super) fn matched_openraft_workload_experiment() {
    let _admission = sample_gate::enter("openraft");
    openraft::matched_workload_experiment();
}

pub(super) fn raft_failure_component() {
    raft_rs::failure_component();
}
pub(super) fn openraft_failure_component() {
    openraft::failure_component();
}
pub(super) fn raft_scale_component() {
    raft_rs::scale_component();
}
pub(super) fn openraft_scale_component() {
    openraft::scale_component();
}
