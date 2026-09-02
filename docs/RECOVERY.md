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

A recovery key is generated when you ask for one in the interactive `cass pages`
wizard (the "recovery key backup" step), or added later to an existing bundle:

```bash
# During creation: run the wizard and accept the recovery-key step
cass pages

# Add to an existing exported bundle (authenticates with the current password)
cass pages key add-recovery --archive ./bundle
```

`--archive` accepts the exported bundle root (the directory that contains `site/`)
or the `site/` directory itself.

### Recovery Secret Format

Recovery secrets are 256 bits (32 bytes) encoded as base64url without padding:

```
Example: q7w8e9r0t1y2u3i4o5p6a7s8d9f0g1h2j3k4l5z6x7c8v9b0
```

**Important:** Store this secret securely. Anyone with the recovery secret can decrypt the archive.

### The Secret Is Shown Exactly Once

The recovery secret is never written into the archive — only a wrapped copy of
the data-encryption key that the secret can unwrap. cass therefore cannot show it
again later: there is no "show recovery" command. The wizard prints it (and can
render it as a QR code when the `qr` feature is built in) at creation, and
`cass pages key add-recovery` prints it once on stdout. Save it before you close
the terminal. If it is lost, add a new recovery slot with
`cass pages key add-recovery` and revoke the old slot.

### Using a Recovery Key

Recovery keys unlock the archive in the published viewer, not on the command line:
open the archive page, choose "Use Recovery Key" on the unlock screen, and enter
the secret exactly as saved. The bundle's generated `recovery.html` carries the
same instructions for people who receive the link without this guide.

The `cass pages key …` commands authenticate with a *password*; a recovery
secret cannot drive them. To regain command-line control of a bundle whose
password is lost, see "Scenario: Forgotten Password" below.

---

## Multi-Key-Slot Operations

All key commands take `--archive <bundle>`, `--json` (one JSON document on
stdout, `success: true`, an `action` tag, and the engine's result fields), and
`--password-stdin`. Passwords are never accepted on the command line. Without
`--password-stdin`, a verb that needs a password prompts for it when stdin is a
terminal and fails with exit 6 (`password-required`) otherwise.

`--password-stdin` reads one password per line: the current password first, then
the new password for `add-password` and `rotate`.

Exit codes: 0 success · 1 the engine refused (wrong password, last slot, unsupported
bundle) · 2 usage · 3 the path is not a pages bundle · 6 no password available.

### Listing Key Slots

`list` never needs a password.

```bash
cass pages key list --archive ./bundle
cass pages key list --archive ./bundle --json | jq '.active_slots'
```

Output:
```
Key slots for /path/to/bundle/site
  export id: …   active slots: 2   dek created: 2026-09-01T20:14:02Z
  slot   0  password   kdf: argon2id
  slot   1  recovery   kdf: hkdf-sha256
```

### Adding a Password Slot

Add an additional password to an existing archive:

```bash
# interactive: prompts for the current password, the new password, and a confirmation
cass pages key add-password --archive ./bundle

# non-interactive
printf '%s\n%s\n' "$CURRENT" "$NEW" | cass pages key add-password --archive ./bundle --password-stdin --json
```

### Adding a Recovery Slot

Add a recovery key to an existing archive:

```bash
cass pages key add-recovery --archive ./bundle
printf '%s\n' "$CURRENT" | cass pages key add-recovery --archive ./bundle --password-stdin --json
```

**Save the displayed recovery secret immediately** (`recovery_secret` in the JSON
document). It is shown once and is not stored.

### Revoking a Key Slot

Remove a key slot:

```bash
cass pages key revoke --archive ./bundle --slot 1
```

**Constraints (enforced by the engine):**
- Cannot revoke the last remaining slot — add another slot first
- Cannot revoke the slot whose password you are authenticating with
- Unknown slot ids are refused
- Revoked slot IDs are never reused

### Key Rotation

Full key rotation regenerates the DEK and re-encrypts all data under a new
password; every previous slot is discarded:

```bash
cass pages key rotate --archive ./bundle
printf '%s\n%s\n' "$CURRENT" "$NEW" | cass pages key rotate --archive ./bundle --password-stdin --keep-recovery --json
```

Options:
- `--keep-recovery`: also mint a new recovery secret for the rotated key (printed once)
- Default: the rotated archive has a single password slot

**When to rotate:**
- Suspected key compromise
- Personnel changes
- Regular security hygiene

---

## Disaster Recovery

### Scenario: Forgotten Password

If you have a recovery key, the archive is still readable: open the published
viewer, choose "Use Recovery Key", and enter the secret. Readers never need the
password.

Command-line key management (`cass pages key …`) authenticates with a password
only, so with the password lost you cannot add or revoke slots on that bundle.
To get back to a bundle you control:

1. Export the archive again with `cass pages` and choose a new password (and a
   new recovery key) in the wizard.
2. Publish the new bundle in place of the old one.
3. Keep the old bundle only as long as its recovery secret is safely stored.

If you have neither the password nor a recovery key, the data in that bundle is
unrecoverable by design; re-export from your local cass archive instead.

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
cass pages --verify ./bundle
cass pages --verify ./bundle --json
```

If specific chunks are corrupted:
- Restore from backup
- If backup unavailable, data in corrupted chunks is lost

### Scenario: Missing Files

The same verifier checks file presence and hashes:

```bash
cass pages --verify ./bundle
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
