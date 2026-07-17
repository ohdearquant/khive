# Formal ontology edge rules

`FormalPack` contributes 21 additive endpoint rules for formal-mathematics concepts while leaving
the base entity-kind and relation enums unchanged.

## Endpoint typing

Every endpoint uses `EndpointKind::EntityOfType { kind: "concept", entity_type: ... }`. A match
therefore requires both the base `concept` kind and one of six registered subtypes: `theorem`,
`definition`, `structure`, `instance`, `axiom`, or `goal`. Matching on subtype text alone is not
sufficient.

## `depends_on`

Fourteen rules encode prerequisite direction from consumer to prerequisite. Theorems may depend on
theorems, definitions, structures, or axioms; definitions may depend on definitions, structures,
theorems, or axioms; instances may depend on structures or definitions; goals may depend on
theorems, definitions, structures, or axioms.

## Structural relations

`instance_of` allows an instance to name its structure. `extends` allows structure-to-structure and
definition-to-definition inheritance. `variant_of` covers theorem restatements, definition
variants, and goals that restate either a theorem or definition.

## Pack runtime

`FormalPack::new` binds a runtime handle. The pack depends only on `kg`, registers no note/entity
kinds of its own, has no handlers or schema plan, and returns `RuntimeError::InvalidInput` for any
dispatch attempt. Its value is the edge-rule extension exposed through shared KG `link` validation.
