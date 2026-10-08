-- `catalog`, `provenance` and `unearth` match their selector flags without
-- regard to case (`--vendor UniFi` finds `unifi`). SQLite only uses an index
-- for a `COLLATE NOCASE` comparison if the index was built with that
-- collation, so the exact-case indexes above don't help. Identity is
-- unchanged: the UNIQUE key and every write still compare exactly.
CREATE INDEX IF NOT EXISTS idx_firmware_current_vendor_nocase
    ON firmware_current (vendor COLLATE NOCASE, device_family COLLATE NOCASE);
