//! The adapter's secrets and its credential: the installation's Ed25519
//! key (a hex seed file, as Hub's `hub admin keygen` writes it), the
//! namespace-signed installation credential (an encoded `SignedRecord`, as
//! `hub admin credential` writes it) and BBS's integration credential. Secret
//! files must be owner-only; their contents are never logged.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use bbs_core::hub_proto::msgproto_v1 as mp;
use ed25519_dalek::{SigningKey, VerifyingKey};

use crate::store::check_private;
use crate::verify::{self, Credential};
use crate::wire;

/// Generate an installation key, write its hex seed to `out` (owner-only,
/// never overwriting) and return its public key.
pub fn generate(out: &Path) -> Result<VerifyingKey> {
    let seed: [u8; 32] = rand::random();
    let key = SigningKey::from_bytes(&seed);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(out)
        .with_context(|| format!("creating {} (it must not exist)", out.display()))?;
    f.write_all(format!("{}\n", data_encoding::HEXLOWER.encode(&seed)).as_bytes())?;
    f.sync_all()?;
    Ok(key.verifying_key())
}

/// Read an installation key's hex seed file, which must be owner-only.
pub fn read_key(path: &Path) -> Result<SigningKey> {
    check_private(path)?;
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let seed = data_encoding::HEXLOWER_PERMISSIVE
        .decode(text.trim().as_bytes())
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .with_context(|| format!("{} is not a 32-byte hex Ed25519 seed", path.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Read BBS's integration credential, which must be owner-only.
pub fn read_token(path: &Path) -> Result<String> {
    check_private(path)?;
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let token = text.trim();
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        bail!("{} does not hold an integration credential", path.display());
    }
    Ok(token.to_string())
}

/// Read this installation's credential and check it is for `namespace` and
/// `installation_id`, certifies `key`, and grants both permissions the
/// adapter uses: ASSERT_SENDERS for messages and directory queries,
/// HOST_NAMESPACE for the route and directory answers.
pub fn read_credential(
    path: &Path,
    namespace: &str,
    installation_id: &str,
    key: &SigningKey,
) -> Result<(mp::SignedRecord, Credential)> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let rec: mp::SignedRecord = wire::decode(&raw, wire::MAX_CREDENTIAL * 2)
        .with_context(|| format!("{} is not an encoded SignedRecord", path.display()))?;
    let c = verify::own_credential(&rec)
        .map_err(|r| anyhow::anyhow!("installation credential {}: {r}", path.display()))?;
    if c.namespace != namespace || c.installation_id != installation_id {
        bail!(
            "installation credential {} is for {}/{}, not {namespace}/{installation_id}",
            path.display(),
            c.namespace,
            c.installation_id
        );
    }
    if c.public_key != key.verifying_key().as_bytes() {
        bail!(
            "installation credential {} certifies another key than the configured one",
            path.display()
        );
    }
    if !c.assert_senders || !c.host_namespace {
        bail!(
            "installation credential {} must grant both assert_senders and host_namespace",
            path.display()
        );
    }
    Ok((rec, c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sign;
    use prost::Message as _;
    use std::os::unix::fs::PermissionsExt as _;

    pub fn credential_for(
        authority: &SigningKey,
        namespace: &str,
        installation: &str,
        subject: &VerifyingKey,
        perms: &[mp::InstallationPermission],
    ) -> Vec<u8> {
        let payload = mp::InstallationCredential {
            namespace: namespace.into(),
            installation_id: installation.into(),
            public_key: subject.as_bytes().to_vec(),
            permissions: perms.iter().map(|p| *p as i32).collect(),
        }
        .encode_to_vec();
        sign::sign(authority, mp::RecordKind::InstallationCredential, payload).encode_to_vec()
    }

    #[test]
    fn keygen_writes_an_owner_only_seed_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install.key");
        let public = generate(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let key = read_key(&path).unwrap();
        assert_eq!(key.verifying_key(), public);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.trim().len(), 64);
        assert!(
            generate(&path).is_err(),
            "an existing key is never replaced"
        );
    }

    #[test]
    fn secrets_others_can_read_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("install.key");
        generate(&key).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();
        let err = read_key(&key).unwrap_err().to_string();
        assert!(err.contains("accessible to other users"), "{err}");

        let token = dir.path().join("integration.token");
        std::fs::write(&token, "secret-token\n").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token(&token).is_err());
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token(&token).unwrap(), "secret-token");
        // An error never shows the secret.
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o604)).unwrap();
        assert!(
            !read_token(&token)
                .unwrap_err()
                .to_string()
                .contains("secret-token")
        );
    }

    #[test]
    fn credentials_must_match_the_installation_and_grant_both_permissions() {
        use mp::InstallationPermission::{AssertSenders, HostNamespace};
        let dir = tempfile::tempdir().unwrap();
        let authority = SigningKey::from_bytes(&[1; 32]);
        let key = SigningKey::from_bytes(&[2; 32]);
        let path = dir.path().join("install.cred");
        let write = |bytes: Vec<u8>| std::fs::write(&path, bytes).unwrap();

        write(credential_for(
            &authority,
            "bbs",
            "bbs-1",
            &key.verifying_key(),
            &[AssertSenders, HostNamespace],
        ));
        let (_, c) = read_credential(&path, "bbs", "bbs-1", &key).unwrap();
        assert!(c.assert_senders && c.host_namespace);
        assert!(read_credential(&path, "hub", "bbs-1", &key).is_err());
        assert!(read_credential(&path, "bbs", "bbs-2", &key).is_err());
        let other = SigningKey::from_bytes(&[3; 32]);
        assert!(read_credential(&path, "bbs", "bbs-1", &other).is_err());

        write(credential_for(
            &authority,
            "bbs",
            "bbs-1",
            &key.verifying_key(),
            &[AssertSenders],
        ));
        assert!(read_credential(&path, "bbs", "bbs-1", &key).is_err());

        let mut tampered = credential_for(
            &authority,
            "bbs",
            "bbs-1",
            &key.verifying_key(),
            &[AssertSenders, HostNamespace],
        );
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        write(tampered);
        assert!(read_credential(&path, "bbs", "bbs-1", &key).is_err());
    }
}
