//! WebKit subprocess support.
//!
//! `proto`  — wire types shared between daemon and worker.
//! `worker` — the subprocess entry point (`br webkit-worker`).
//!
//! Daemon-side `WorkerHandle` (Phase 2) lives here too, but isn't needed
//! for Phase 1 — the worker can be smoke-tested directly by piping JSON
//! into stdin.

pub mod handle;
pub mod proto;
pub mod worker;
