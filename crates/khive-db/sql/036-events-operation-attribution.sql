-- Historical events have no proven parser position or reference provenance.
ALTER TABLE events ADD COLUMN op_index INTEGER
    CHECK (op_index IS NULL OR op_index BETWEEN 0 AND 4294967295);
ALTER TABLE events ADD COLUMN ref_resolution TEXT
    CHECK ((op_index IS NULL AND ref_resolution IS NULL) OR
           (op_index IS NOT NULL AND ref_resolution IS NOT NULL AND
            ref_resolution IN ('literal', 'resolved')));
