# Formal-mathematics ontology design

Formal knowledge fits the existing graph substrate when mathematical roles are concept subtypes and
their relationships use the closed shared relation set. The formal pack therefore changes endpoint
validation rather than adding tables, verbs, entity kinds, or relation variants.

The direction of `depends_on` follows proof dependency: a theorem or goal points toward the facts
and structures needed to establish it. `instance_of` captures a value realizing a structure,
`extends` captures definitional inheritance, and `variant_of` makes restatement relationships
queryable without treating duplicates as unrelated work.

Additive rules broaden what shared `link` accepts without invalidating base behavior. Requiring the
full `(concept, entity_type)` pair prevents a coincidental property value on another base entity kind
from entering the formal ontology.
