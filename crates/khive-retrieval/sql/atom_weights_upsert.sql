INSERT INTO atom_weights (namespace, atom_id, weight, updated_at, version)
VALUES (?1, ?2, ?3, ?4, 1)
ON CONFLICT(namespace, atom_id) DO UPDATE SET
    weight     = excluded.weight,
    updated_at = excluded.updated_at,
    version    = version + 1
