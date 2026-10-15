//! The wire contract is checked on every request, independently of discovery.

use anyhow::{Result, bail};
use semver::Version;
use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::{Response, Status};

use crate::{proto, release};

pub const EPOCH: u32 = 2;
pub const CAPABILITIES: &[&str] = &[
    "named-agents.v1",
    "owner-attribution.v1",
    "human-token-sessions.v1",
    "group-archive.v1",
    "group-description.v1",
    "feed-filter.v1",
];
const MAX_METADATA_BYTES: usize = 1024;
const MAX_VERSION_BYTES: usize = 128;
const REQUEST_KEYS: &[&str] = &[
    "bbs-client-version",
    "bbs-protocol-epoch",
    "bbs-required-capabilities",
];

#[derive(Clone, Default)]
pub struct Policy {
    minimum: Option<Version>,
    recommended: Option<Version>,
}

impl Policy {
    pub fn new(minimum: Option<Version>, recommended: Option<Version>) -> Result<Self> {
        for version in minimum.iter().chain(recommended.iter()) {
            if version.to_string().len() > MAX_VERSION_BYTES {
                bail!("client version policy exceeds {MAX_VERSION_BYTES} bytes");
            }
        }
        if let (Some(minimum), Some(recommended)) = (&minimum, &recommended)
            && recommended.cmp_precedence(minimum).is_lt()
        {
            bail!("recommended client version must not be below the minimum client version");
        }
        Ok(Self {
            minimum,
            recommended,
        })
    }

    pub fn info(&self, require_human_token: bool) -> proto::ServerInfoResponse {
        proto::ServerInfoResponse {
            require_human_token,
            protocol_epoch: Some(EPOCH),
            capabilities: CAPABILITIES.iter().map(|s| (*s).into()).collect(),
            release: Some(release::info()),
            minimum_client_version: self.minimum.as_ref().map(ToString::to_string),
            recommended_client_version: self.recommended.as_ref().map(ToString::to_string),
        }
    }

    fn hints(&self, metadata: &mut MetadataMap) {
        metadata.insert(
            "bbs-server-version",
            MetadataValue::from_static(release::VERSION),
        );
        metadata.insert("bbs-protocol-epoch", MetadataValue::from_static("2"));
        if let Some(version) = &self.minimum {
            metadata.insert(
                "bbs-minimum-client-version",
                version.to_string().parse().expect("SemVer is ASCII"),
            );
        }
        if let Some(version) = &self.recommended {
            metadata.insert(
                "bbs-recommended-client-version",
                version.to_string().parse().expect("SemVer is ASCII"),
            );
        }
    }

    pub fn status(&self, mut status: Status) -> Status {
        self.hints(status.metadata_mut());
        status
    }

    pub fn finish<T>(&self, result: Result<Response<T>, Status>) -> Result<Response<T>, Status> {
        result
            .map(|mut response| {
                self.hints(response.metadata_mut());
                response
            })
            .map_err(|status| self.status(status))
    }

    pub fn validate(&self, metadata: &MetadataMap) -> Result<(), Status> {
        self.validate_inner(metadata)
            .map_err(|status| self.status(status))
    }

    fn validate_inner(&self, metadata: &MetadataMap) -> Result<(), Status> {
        let mut size = 0;
        for key in REQUEST_KEYS {
            let values = metadata.get_all(*key);
            for value in values.iter() {
                size += key.len() + value.as_encoded_bytes().len();
            }
            if values.iter().count() > 1 {
                return Err(Status::invalid_argument(format!(
                    "duplicate {key} metadata"
                )));
            }
        }
        if size > MAX_METADATA_BYTES {
            return Err(Status::invalid_argument(
                "BBS compatibility metadata exceeds 1 KiB",
            ));
        }
        let get = |key| -> Result<Option<&str>, Status> {
            metadata
                .get(key)
                .map(|v| {
                    v.to_str()
                        .map_err(|_| Status::invalid_argument("invalid BBS compatibility metadata"))
                })
                .transpose()
        };
        if get("bbs-protocol-epoch")? != Some("2") {
            return Err(Status::failed_precondition(
                "incompatible BBS protocol: this server requires epoch 2 (bbs.v2); upgrade the client and server together",
            ));
        }
        if let Some(required) = get("bbs-required-capabilities")? {
            for capability in required.split(',') {
                if !valid_capability(capability) {
                    return Err(Status::invalid_argument(
                        "malformed required BBS capability",
                    ));
                }
                if !CAPABILITIES.contains(&capability) {
                    return Err(Status::failed_precondition(format!(
                        "incompatible BBS server: required capability {capability} is unavailable; upgrade the server or use a compatible client",
                    )));
                }
            }
        }
        let version = get("bbs-client-version")?
            .map(parse_version)
            .transpose()
            .map_err(|_| {
                Status::invalid_argument("malformed bbs-client-version; expected SemVer")
            })?;
        if let Some(minimum) = &self.minimum
            && version
                .as_ref()
                .is_none_or(|v| v.cmp_precedence(minimum).is_lt())
        {
            return Err(Status::failed_precondition(format!(
                "incompatible BBS client: server requires version {minimum} or newer; upgrade the client",
            )));
        }
        Ok(())
    }
}

fn parse_version(value: &str) -> Result<Version> {
    if value.len() > MAX_VERSION_BYTES {
        bail!("version is too long");
    }
    Ok(Version::parse(value)?)
}

fn valid_capability(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

pub fn client_metadata(metadata: &mut MetadataMap, capabilities: &[&str]) {
    metadata.insert(
        "bbs-client-version",
        MetadataValue::from_static(release::VERSION),
    );
    metadata.insert("bbs-protocol-epoch", MetadataValue::from_static("2"));
    if !capabilities.is_empty() {
        metadata.insert(
            "bbs-required-capabilities",
            capabilities
                .join(",")
                .parse()
                .expect("capability constants are ASCII"),
        );
    }
}

/// Discovery is diagnostic, never permission to omit request-level requirements.
pub fn validate_info(info: &proto::ServerInfoResponse) -> Result<()> {
    if info.protocol_epoch != Some(EPOCH) {
        bail!(
            "incompatible BBS ServerInfo: expected protocol epoch {EPOCH}; coordinate a client/server upgrade"
        );
    }
    let release = info
        .release
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("invalid BBS ServerInfo: missing release identity"))?;
    parse_version(&release.version)
        .map_err(|_| anyhow::anyhow!("invalid BBS ServerInfo release version"))?;
    if release.revision.len() > 128 || release.revision.is_empty() {
        bail!("invalid BBS ServerInfo revision");
    }
    if info.capabilities.len() > 32 || info.capabilities.iter().any(|c| !valid_capability(c)) {
        bail!("invalid BBS ServerInfo capabilities");
    }
    for version in info
        .minimum_client_version
        .iter()
        .chain(info.recommended_client_version.iter())
    {
        parse_version(version)
            .map_err(|_| anyhow::anyhow!("invalid BBS ServerInfo client version policy"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_and_unsupported_requests_are_rejected() {
        let policy = Policy::default();
        let valid = || {
            let mut m = MetadataMap::new();
            client_metadata(&mut m, CAPABILITIES);
            m
        };
        policy.validate(&valid()).unwrap();
        for epoch in [None, Some("1"), Some("3"), Some("02"), Some("garbage")] {
            let mut m = valid();
            m.remove("bbs-protocol-epoch");
            if let Some(epoch) = epoch {
                m.insert("bbs-protocol-epoch", epoch.parse().unwrap());
            }
            let error = policy.validate(&m).unwrap_err();
            assert_eq!(error.code(), tonic::Code::FailedPrecondition);
            assert_eq!(error.metadata().get("bbs-protocol-epoch").unwrap(), "2");
        }
        for key in REQUEST_KEYS {
            let mut m = valid();
            let duplicate = m.get(*key).unwrap().clone();
            m.append(*key, duplicate);
            assert_eq!(
                policy.validate(&m).unwrap_err().code(),
                tonic::Code::InvalidArgument
            );
        }
        for capabilities in [
            "",
            "named-agents.v1,",
            "named-agents.v1, future.v1",
            "named-agents.v1,future.v1",
        ] {
            let mut m = valid();
            m.insert("bbs-required-capabilities", capabilities.parse().unwrap());
            assert!(policy.validate(&m).is_err());
        }
        let mut m = valid();
        m.insert(
            "bbs-required-capabilities",
            "named-agents.v1,".repeat(100).parse().unwrap(),
        );
        assert_eq!(
            policy.validate(&m).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn minimum_versions_use_semver_precedence() {
        let policy = Policy::new(
            Some("0.2.10".parse().unwrap()),
            Some("0.3.0".parse().unwrap()),
        )
        .unwrap();
        for (version, allowed) in [
            (None, false),
            (Some("broken"), false),
            (Some("0.2.9"), false),
            (Some("0.2.10-rc.1"), false),
            (Some("0.2.10"), true),
            (Some("0.2.10+build"), true),
            (Some("0.3.0"), true),
        ] {
            let mut m = MetadataMap::new();
            m.insert("bbs-protocol-epoch", "2".parse().unwrap());
            if let Some(version) = version {
                m.insert("bbs-client-version", version.parse().unwrap());
            }
            assert_eq!(policy.validate(&m).is_ok(), allowed, "{version:?}");
        }
        assert!(
            Policy::new(
                Some("2.0.0".parse().unwrap()),
                Some("1.0.0".parse().unwrap())
            )
            .is_err()
        );
        let mut m = MetadataMap::new();
        m.insert("bbs-protocol-epoch", "2".parse().unwrap());
        Policy::default().validate(&m).unwrap();
    }

    #[test]
    fn info_requires_present_contract_and_accepts_future_optional_capabilities() {
        let mut info = Policy::default().info(false);
        validate_info(&info).unwrap();
        info.capabilities.push("future.v1".into());
        validate_info(&info).unwrap();
        info.protocol_epoch = None;
        assert!(validate_info(&info).is_err());
        info.protocol_epoch = Some(EPOCH);
        info.release = None;
        assert!(validate_info(&info).is_err());
    }
}
