//! Partial native adapter probes; the frozen complete M02.10 matrix remains required.
mod durability;

#[test]
fn raft_ready_persistence_probe() {
    durability::raft_ready_persistence_probe();
}

#[test]
fn openraft_persistence_probe() {
    durability::openraft_persistence_probe();
}

#[test]
fn fixed_offered_admission_controls() {
    durability::fixed_offered_admission_controls();
}

#[test]
fn matched_raft_workload_experiment() {
    durability::matched_raft_workload_experiment();
}

#[test]
fn matched_openraft_workload_experiment() {
    durability::matched_openraft_workload_experiment();
}

#[test]
fn raft_failure_membership_component() {
    durability::raft_failure_component();
}

#[test]
fn openraft_failure_membership_component() {
    durability::openraft_failure_component();
}

#[test]
fn raft_active_group_scale_component() {
    durability::raft_scale_component();
}

#[test]
fn openraft_active_group_scale_component() {
    durability::openraft_scale_component();
}
