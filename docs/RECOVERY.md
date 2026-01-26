# Recovery Guide for Encrypted Pages Archives

This document covers recovery procedures for encrypted cass Pages archives.

## Table of Contents

1. [Key Architecture](#key-architecture)
2. [Recovery Key Basics](#recovery-key-basics)
3. [Multi-Key-Slot Operations](#multi-key-slot-operations)
4. [Disaster Recovery](#disaster-recovery)
5. [Best Practices](#best-practices)
6. [Troubleshooting](#troubleshooting)

---

## Key Architecture

Cass Pages archives use envelope encryption with a LUKS-like key slot system:

```
┌─────────────────────────────────────────┐
│              config.json                │
├─────────────────────────────────────────┤
│  Key Slot 0 (Password)                  │
│  ├─ KEK derived via Argon2id            │
│  └─ Wrapped DEK                         │
├─────────────────────────────────────────┤
│  Key Slot 1 (Recovery)                  │
│  ├─ KEK derived via HKDF-SHA256         │
│  └─ Wrapped DEK                         │
├─────────────────────────────────────────┤
│  Payload Metadata                       │
│  └─ chunk_count, base_nonce, etc.       │
└─────────────────────────────────────────┘

                    │
                    ▼

┌─────────────────────────────────────────┐
│              payload/                    │
│  chunk-00000.bin  ─────────────┐        │
│  chunk-00001.bin               │        │
│  ...                           │        │
└────────────────────────────────│────────┘
                                 │
                     Encrypted with DEK
                     (AES-256-GCM)
```

### Key Components

| Component | Description | Algorithm |
|-----------|-------------|-----------|
| DEK | Data Encryption Key (32 bytes) | Random |
| KEK | Key Encryption Key (32 bytes) | Derived from password/recovery |
| Wrapped DEK | DEK encrypted with KEK | AES-256-GCM |
| Salt | Per-slot random salt | 32 bytes (password) / 16 bytes (recovery) |
| Nonce | Per-slot random nonce | 12 bytes |

### Password Slots

Password-based key slots use **Argon2id** for key derivation:

- Memory: 64 MB
- Iterations: 3
- Parallelism: 4
- Output: 32 bytes (256-bit KEK)

### Recovery Slots

Recovery key slots use **HKDF-SHA256** for key derivation:

- Input: 256-bit random secret
- Salt: 16 bytes random
- Info: `cass-pages-kek-v2`
- Output: 32 bytes (256-bit KEK)

---

## Recovery Key Basics

### Generating a Recovery Key

Recovery keys are generated during archive creation or can be added later:

```bash
# During creation with wizard
cass pages encrypt archive.db --with-recovery

# Add to existing archive
cass pages key add-recovery --archive ./archive
```

### Recovery Secret Format

Recovery secrets are 256 bits (32 bytes) encoded as base64url without padding:

```
Example: q7w8e9r0t1y2u3i4o5p6a7s8d9f0g1h2j3k4l5z6x7c8v9b0
```

**Important:** Store this secret securely. Anyone with the recovery secret can decrypt the archive.

### QR Code Generation

Recovery secrets can be displayed as QR codes for offline backup:

```bash
cass pages key show-recovery --archive ./archive --qr
```

The QR code contains the base64url-encoded secret and can be scanned to restore access.

### Using a Recovery Key

To unlock an archive with a recovery key:

```bash
# Interactive
cass pages decrypt ./archive

# Programmatic (stdin)
echo "base64url-secret-here" | cass pages decrypt ./archive --recovery-stdin
```

---

## Multi-Key-Slot Operations

### Listing Key Slots

```bash
cass pages key list --archive ./archive
```

Output:
```
Key Slots:
  Slot 0: password (Argon2id)
  Slot 1: recovery (HKDF-SHA256)

Active slots: 2
```

### Adding a Password Slot

Add an additional password to an existing archive:

```bash
cass pages key add-password --archive ./archive
```

You'll be prompted for:
1. Current password (to authenticate)
2. New password (to add)

### Adding a Recovery Slot

Add a recovery key to an existing archive:

```bash
cass pages key add-recovery --archive ./archive
```

**Save the displayed recovery secret immediately.**

### Revoking a Key Slot

Remove a key slot:

```bash
cass pages key revoke --archive ./archive --slot 1
```

**Constraints:**
- Cannot revoke the last remaining slot
- Cannot revoke the slot you're authenticating with
- Revoked slot IDs are never reused

### Key Rotation

Full key rotation regenerates the DEK and re-encrypts all data:

```bash
cass pages key rotate --archive ./archive
```

Options:
- `--keep-recovery`: Generate new recovery key after rotation
- Default: Creates single password slot

**When to rotate:**
- Suspected key compromise
- Personnel changes
- Regular security hygiene

---

## Disaster Recovery

### Scenario: Forgotten Password

If you have a recovery key:

```bash
cass pages decrypt ./archive --recovery
# Enter recovery secret when prompted
```

Then add a new password:

```bash
cass pages key add-password --archive ./archive
```

### Scenario: Corrupted config.json

Symptoms:
- "Failed to parse config" errors
- "Invalid JSON" errors

Recovery steps:

1. **Check for backup:** Look for `config.json.bak` or version control
2. **Restore from backup:** Copy backup over corrupted file
3. **If no backup:** Archive is likely unrecoverable without config.json

Prevention: Always keep backups of encrypted archives.

### Scenario: Corrupted Payload Chunks

Symptoms:
- "Authentication failed" during decryption
- "Invalid chunk" errors

Verification:

```bash
cass pages verify --archive ./archive
```

If specific chunks are corrupted:
- Restore from backup
- If backup unavailable, data in corrupted chunks is lost

### Scenario: Missing Files

Use integrity verification:

```bash
cass pages verify --archive ./archive --check-integrity
```

This validates:
- All files listed in config.json exist
- SHA-256 hashes match integrity.json (if present)

---

## Best Practices

### Backup Strategy

1. **Store recovery key offline:** Print QR code, store in safe
2. **Backup entire archive:** Include config.json and all payload chunks
3. **Test recovery regularly:** Verify you can decrypt with recovery key
4. **Geographic distribution:** Store backups in multiple locations

### Key Management

1. **Use strong passwords:** Minimum 12 characters, mixed case/numbers/symbols
2. **Limit key slots:** Only create slots you need
3. **Revoke unused slots:** Remove access when no longer needed
4. **Rotate after incidents:** Change keys if compromise suspected

### Verification Checklist

Before relying on an archive:

- [ ] Password unlocks archive
- [ ] Recovery key unlocks archive
- [ ] `cass pages verify` passes
- [ ] Backup copy exists and is verified
- [ ] Recovery secret stored securely offline

---

## Troubleshooting

### Error: "Invalid password or no matching key slot"

**Causes:**
- Typo in password
- Wrong password
- Password slot was revoked

**Solutions:**
- Try recovery key
- Check for password manager entry
- Verify slot exists with `key list`

### Error: "Cannot revoke the last remaining key slot"

**Cause:** Attempting to revoke the only active slot

**Solution:** Add another slot first, then revoke

### Error: "Cannot revoke slot used for authentication"

**Cause:** Trying to revoke the slot you authenticated with

**Solution:** Use a different password/recovery to authenticate

### Error: "Key unwrapping failed"

**Causes:**
- Corrupted wrapped_dek
- Wrong password/recovery key
- Modified config.json

**Solutions:**
- Try different credentials
- Restore config.json from backup
- Use recovery key if available

### Error: "Chunk authentication failed"

**Cause:** Payload chunk was modified or corrupted

**Solutions:**
- Restore chunk from backup
- If backup unavailable, that chunk's data is lost

### Error: "Missing chunk file"

**Cause:** Payload file was deleted or not copied

**Solution:** Restore from backup

---

## Security Considerations

### What Recovery Keys Provide

Recovery keys provide full access to archive contents, equivalent to the primary password. They are designed for:

- Emergency access when password is forgotten
- Backup administrators
- Estate planning

### What Recovery Keys Don't Protect Against

- Compromised recovery key
- Corrupted payload data
- Deleted archive files

### Secure Storage

Store recovery keys:
- Printed and sealed in safe deposit box
- Hardware security module (HSM)
- Password manager with separate master password
- Split across multiple locations (Shamir's Secret Sharing)

**Never store:**
- In plaintext files
- In email
- In cloud storage without additional encryption
- On the same device as the archive
