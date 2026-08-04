# Version History

This document tracks schema versions and breaking changes for cass archives and databases.

## Schema Versions

### Database Schema

The internal database uses a versioned schema stored in the `meta` table. The schema version is checked on open and automatically migrated when possible.

| Version | Date | Changes |
|---------|------|---------|
| 8 | Current | Added source provenance tracking, multi-source support |
| 7 | 2024-Q4 | Added snippet support, code FTS indexing |
| 6 | 2024-Q4 | Added workspace display names |
| 5 | 2024-Q3 | Initial stable schema |

#### Migration Support

- **Forward compatible**: Databases from schema v5+ can be opened by current code
- **Automatic migration**: v5-v7 databases are automatically upgraded
- **Rebuild required**: v0-v4 databases require full reindex

### Encryption Config Schema

Encrypted archives use a versioned config.json format.

| Version | Date | Changes |
|---------|------|---------|
| 2 | Current | Added recovery key slots, HKDF support |
| 1 | 2024-Q3 | Initial format with Argon2id + AES-256-GCM |

## Version Compatibility Matrix

| cass Version | DB Schema | Encryption Format | Notes |
|--------------|-----------|-------------------|-------|
| 0.1.x | 5-8 | 1-2 | Current stable |
| 0.0.x | 1-4 | - | Pre-encryption, requires rebuild |

## Breaking Changes

### 0.1.50 - Source Provenance
- Added `sources` table for tracking import sources
- Migration: Automatic, adds table with default local source

### 0.1.40 - Encryption
- Introduced encrypted export format
- Breaking: Exported archives now require password

### 0.1.30 - Workspace Support
- Added `workspaces` table
- Migration: Automatic, extracts workspaces from conversation paths

## Upgrading

### Database Upgrades

Database upgrades happen automatically when opening with `SqliteStorage::open_with_migration()`:

```rust
use coding_agent_search::storage::sqlite::SqliteStorage;

let storage = SqliteStorage::open_with_migration(&db_path)?;
// Database is now at current schema version
```

### Archive Decryption

Old archives (v1) are compatible with current code:

```rust
use coding_agent_search::pages::encrypt::decrypt_archive;

// Works with both v1 and v2 archives
let data = decrypt_archive(&archive_path, password)?;
```

## Data Preservation

When migration fails:
1. A backup is created at `{db_path}.backup.{timestamp}`
2. Original file is preserved
3. Full reindex is required

## Testing Version Compatibility

```bash
# Run upgrade tests
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-version-target cargo test --test upgrade

# Test specific version migration
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-version-target cargo test test_schema_version_5_to_current
```

## Future Versions

Planned changes for future versions:

### v9 (Planned)
- Semantic search embeddings table
- Vector index metadata

### Encryption v3 (Planned)
- Additional KDF algorithms
- Key rotation support

## FAQ

### Can I open old archives with new versions?
Yes, all encryption v1+ archives are forward compatible.

### What if migration fails?
The original database is backed up. You can restore it or reindex from source files.

### How do I check my database version?

```bash
cass health  # Shows schema version
# or
sqlite3 ~/.local/share/cass/cass.db "SELECT * FROM meta WHERE key='schema_version'"
```

### Can I downgrade?
Not recommended. Newer features may store data incompatible with old versions. Always backup before upgrading.
