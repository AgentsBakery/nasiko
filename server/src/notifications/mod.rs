//! Notification channels: where alerts are delivered (signed webhooks and
//! Slack), how they are routed, and the durable outbox that carries them.
//!
//! `ssrf` decides which destinations are allowed, `payload` renders and signs
//! bodies and `dispatch` is the outbox worker.

pub mod dispatch;
pub mod payload;
pub mod ssrf;
