namespace LeanMini

/-- Add a natural number to itself. -/
def double (n : Nat) : Nat := n + n

/-- A small record with two natural-number fields. -/
structure Pair where
  left : Nat
  right : Nat

/-- Give the record a concrete default value. -/
instance pairInhabited : Inhabited Pair where
  default := { left := 0, right := 0 }

/-- A postulate, deliberately distinct from an incomplete theorem. -/
axiom shift_identity (n : Nat) : n + 0 = n

/-- A theorem whose proof is deliberately unfinished. -/
theorem double_zero : double 0 = 0 := by
  sorry

/-- An anonymous, goal-shaped statement with a complete proof. -/
example (n : Nat) : n = n := by
  rfl

end LeanMini
