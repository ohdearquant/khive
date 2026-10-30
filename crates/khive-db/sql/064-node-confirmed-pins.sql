-- Confirmed public-key history for node channel peers. Rows are retained, never
-- overwritten or deleted: an old key epoch stays readable after a newer one.
CREATE TABLE IF NOT EXISTS comm_node_confirmed_pin (
    namespace          TEXT NOT NULL,
    slug               TEXT NOT NULL,
    realm              TEXT NOT NULL,
    agent_id           TEXT NOT NULL,
    device_id          TEXT NOT NULL,
    key_epoch          INTEGER NOT NULL CHECK (key_epoch BETWEEN 1 AND 4294967295),
    kem_public_key     BLOB NOT NULL CHECK (length(kem_public_key) = 32),
    signing_public_key BLOB NOT NULL CHECK (length(signing_public_key) = 32),
    fingerprint        BLOB NOT NULL CHECK (length(fingerprint) = 32),
    PRIMARY KEY (namespace, slug, realm, agent_id, device_id, key_epoch)
);
