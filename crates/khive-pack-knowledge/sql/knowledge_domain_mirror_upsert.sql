INSERT INTO knowledge_atoms (id, namespace, slug, name, content, tags, properties, status,
  finalized, created_at, updated_at)
VALUES (?1,?2,?3,?4,?5,?6,?7,'reviewed',1,?8,?9)
ON CONFLICT(id) DO UPDATE SET slug=?3, name=?4, content=?5, tags=?6, properties=?7,
  status='reviewed', finalized=1, updated_at=?9
