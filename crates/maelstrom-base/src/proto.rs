//! Messages sent between various binaries.

use crate::{
    stats::{BrokerStatistics, JobStateCounts},
    ClientJobId, JobId, JobSpec, JobStringResult, Sha256Digest, Utf8PathBuf,
};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

/// The first message sent by a connector to the broker. It identifies what the connector is, and
/// provides any relevant information.
#[derive(Serialize, Deserialize, Debug)]
pub enum Hello {
    Client,
    Worker { slots: u32 },
    ArtifactPusher,
    ArtifactFetcher,
}

/// Message sent from the broker to a worker. The broker won't send a message until it has received
/// a [`Hello`] and determined the type of its interlocutor.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum BrokerToWorker {
    EnqueueJob(JobId, JobSpec),
    CancelJob(JobId),
}

/// Message sent from a worker to the broker. These are responses to previous
/// [`BrokerToWorker::EnqueueJob`] messages. After sending the initial [`Hello`], a worker will
/// send a stream of these messages.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WorkerToBroker(pub JobId, pub JobStringResult);

/// Message sent from the broker to a client. The broker won't send a message until it has recevied
/// a [`Hello`] and determined the type of its interlocutor.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum BrokerToClient {
    JobResponse(ClientJobId, JobStringResult),
    TransferArtifact(Sha256Digest),
    StatisticsResponse(BrokerStatistics),
    JobStateCountsResponse(JobStateCounts),
}

/// Message sent from a client to the broker. After sending the initial [`Hello`], a client will
/// send a stream of these messages.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ClientToBroker {
    JobRequest(ClientJobId, JobSpec),
    StatisticsRequest,
    JobStateCountsRequest,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum Identity {
    Name(String),
    Id(u64),
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct Mode(u32);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct UnixTimestamp(u64);

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ManifestEntryMetadata {
    pub size: u64,
    pub mode: Mode,
    pub user: Identity,
    pub group: Identity,
    pub mtime: UnixTimestamp,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ManifestEntryData {
    Directory,
    File(Option<Sha256Digest>),
    Symlink(Vec<u8>),
    Hardlink(Utf8PathBuf),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ManifestEntry {
    pub path: Utf8PathBuf,
    pub metadata: ManifestEntryMetadata,
    pub data: ManifestEntryData,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u32)]
pub enum ManifestVersion {
    #[default]
    V0 = 0,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub enum ArtifactType {
    /// A .tar file
    Tar,
    /// A serialized `Manifest`
    Manifest,
    /// Binary blob used by manifests
    Binary,
}

impl ArtifactType {
    pub fn try_from_extension(ext: &str) -> Option<Self> {
        match ext {
            "tar" => Some(Self::Tar),
            "manifest" => Some(Self::Manifest),
            "bin" => Some(Self::Binary),
            _ => None,
        }
    }

    pub fn ext(&self) -> &'static str {
        match self {
            Self::Tar => "tar",
            Self::Manifest => "manifest",
            Self::Binary => "bin",
        }
    }
}

/// Metadata about an artifact.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArtifactMetadata {
    pub type_: ArtifactType,
    /// The digest of the contents
    pub digest: Sha256Digest,
    /// The size of the artifact in bytes
    pub size: u64,
}

/// Message sent from the broker to an artifact fetcher. This will be in response to an
/// [`ArtifactFetcherToBroker`] message. On success, the message contains the size of the artifact.
/// The message is then be followed by exactly that many bytes of the artifact's body. On failure,
/// the contains details about what went wrong. After a failure, the broker will close the artifact
/// fetcher connection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BrokerToArtifactFetcher(pub Result<u64, String>);

/// Message sent from an artifact fetcher to the broker. It will be answered with a
/// [`BrokerToArtifactFetcher`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArtifactFetcherToBroker(pub Sha256Digest);

/// Message sent from the broker to an artifact pusher. This will be in response to an
/// [`ArtifactPusherToBroker`] message and the artifact's body. On success, the message contains no
/// other details, indicating that the artifact was successfully written to disk, and that the
/// digest matched. On failure, this message contains details about what went wrong. After a
/// failure, the broker will close the artifact pusher connection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BrokerToArtifactPusher(pub Result<(), String>);

/// Message sent from an artifact pusher to the broker. It contains metadata about the artifact
/// that is about to be pushed. The body of the artifact will immediately follow this message. It
/// will be answered with a [`BrokerToArtifactPusher`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArtifactPusherToBroker(pub ArtifactMetadata);