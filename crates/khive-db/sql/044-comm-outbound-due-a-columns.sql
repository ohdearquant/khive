-- V44: app-maintained strict deadline key and its source text. NULL keys are
-- conservative due candidates; the read path applies the strict residual.
ALTER TABLE notes ADD COLUMN strict_due_key BLOB;
ALTER TABLE notes ADD COLUMN due_source TEXT;
