#![cfg(feature = "fault-injection")]

use khive_runtime::{
    arm_fts_fail_many_partial_scoped, arm_fts_fail_many_scoped, arm_fts_fail_scoped,
    arm_vector_fail_scoped, FaultInjectionArm,
};

#[test]
fn scoped_fault_injection_arms_are_exported() {
    let _: fn(&str) -> FaultInjectionArm = arm_fts_fail_scoped;
    let _: fn(&str) -> FaultInjectionArm = arm_fts_fail_many_scoped;
    let _: fn(&str) -> FaultInjectionArm = arm_fts_fail_many_partial_scoped;
    let _: fn(&str) -> FaultInjectionArm = arm_vector_fail_scoped;
}
