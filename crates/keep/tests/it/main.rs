//! One integration test binary for the keep crate (Rust layout standard).

mod api_surface;
mod backup;
mod benchmark_keep_competitor_performance_meter_gate;
mod ci_contract;
mod collections_api;
mod compatibility;
mod core_reuse;
mod deploy_cli;
mod docs_lint;
mod durability;
mod gke_raft_overlay;
mod http_api;
mod https_api;
mod operator;
mod perf_gate_workflow;
mod properties;
mod raft_failover;
mod raft_membership;
mod raft_node;
mod rolling_restart;
mod security_keep_security_hardening_guard_scan;
mod server_lifecycle_readiness;
mod sift_agnostic_telemetry;
mod spec_cli;
mod structured_stdout_traceparent;
mod test_image_raft;
