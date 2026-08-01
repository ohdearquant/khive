-- V15: persist the tri-state serve_attribution marker on brain_serve_ledger
-- (ADR-081 amendment). Nullable — a legacy row with no marker keeps today's
-- fail-safe: a null accounting_profile_id reclassifies as unattributed
-- regardless of this column. A stored "unspecified" row is the only case
-- permitted to fall back to legacy binding/default resolution when its
-- accounting_profile_id is null.
ALTER TABLE brain_serve_ledger ADD COLUMN serve_attribution TEXT;
