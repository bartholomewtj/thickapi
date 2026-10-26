use uuid::Uuid;

/// Fixed UUID namespace for Claude session mapping (SPEC §10.6). Never change.
pub const THICKAPI_CLAUDE_NS: Uuid = Uuid::from_u128(0x8f3e2a1b_6c4d_4e5f_9a0b_1c2d3e4f5a6b);

/// Manual cooldown for headerless HTTP 429 (SPEC §1.4); cleared via `doctor clear-health`.
pub const SUSPECTED_BLOCKED_COOLDOWN_HOURS: u64 = 4;
