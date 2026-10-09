//! Recovery primitives for restoring note state after device loss.
//!
//! The private-note drain pages the note transport itself, independent of the client's cursors.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use miden_client::note::{NoteFile, NoteScreenerError, NoteSyncHint};
use miden_client::note_transport::{
    NoteInfo, NoteTransportClient, NoteTransportCursor, NoteTransportError,
};
use miden_client::store::NoteFilter;
use miden_client::sync::NoteTagSource;
use miden_client::{ClientError, Deserializable};
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{Note, NoteDetails, NoteId, NoteTag};

use super::MultisigClient;
use crate::MidenSdkClient;
use crate::error::{MultisigError, Result, error_chain};
use crate::rpc::{is_transient_note_transport_error, is_transient_rpc_error};

/// Outcome class of a private-note transport backlog drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportRecoveryStatus {
    /// The account's transport backlog was read to the end; every retained
    /// note the account can consume, minus the skipped ones, is now in the
    /// local store.
    Completed,
    /// The transport could not be consulted at all: it is disabled for this
    /// client (no endpoint configured) or unreachable before anything was
    /// imported (`imported` is always 0 — a connection lost mid-drain after
    /// partial progress reports `Failed` instead). The rest of a recovery
    /// flow should proceed without transport notes.
    Unavailable,
    /// The drain started but did not finish; the backlog may be partially
    /// imported. `retryable` distinguishes transient failures (rerun the
    /// drain) from permanent ones.
    Failed,
}

/// Result of [`MultisigClient::drain_private_note_backlog`].
///
/// Transport problems are reported here rather than returned as errors so a
/// transport failure never aborts the rest of a recovery flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportRecoveryReport {
    /// Outcome class of the drain.
    pub status: TransportRecoveryStatus,
    /// Number of note records newly imported into the local store by this
    /// drain. Can be non-zero even on `Failed`: batches imported before the
    /// failure stay imported.
    pub imported: usize,
    /// Whether rerunning the drain can plausibly succeed (transient transport
    /// or node failures, a transport page that broke pagination). Always
    /// `false` on `Completed`.
    pub retryable: bool,
    /// Human-readable cause when the drain did not complete.
    pub reason: Option<String>,
    /// Malformed notes skipped (undecodable, header mismatch, unrequested tag). The gRPC
    /// transport fails such a page instead, so only other transports report these.
    pub skipped_invalid: usize,
    /// Transport notes skipped because their consumability check failed.
    pub skipped_unscreenable: usize,
}

/// Upper bound on transport pages per tag group. Every drain restarts from
/// the oldest retained note, so hitting it is not retryable.
const MAX_PAGES_PER_TAG_GROUP: usize = 10_000;

enum DrainStop {
    Client(ClientError),
    InvalidPage,
    PageLimit,
}

#[derive(Default)]
struct Skipped {
    invalid: usize,
    unscreenable: usize,
}

impl From<ClientError> for DrainStop {
    fn from(err: ClientError) -> Self {
        Self::Client(err)
    }
}

impl MultisigClient {
    /// Pages the transport for `account_id`'s note tags from its oldest
    /// retained note, independent of the client's sync cursors, so notes an
    /// earlier sync passed over are read again. Notes for other accounts
    /// sharing a tag, notes already resolved locally, and malformed notes are
    /// skipped. Each page is imported before the next is fetched, so a failed
    /// drain keeps its progress and a rerun is idempotent.
    ///
    /// Transport recovery is bounded by the transport service's retention:
    /// senders may bypass the transport entirely and relayed blobs are pruned
    /// after the retention window, so this is a best-effort rescan, **not** a
    /// backup. Transport-disabled and transport-unreachable outcomes are
    /// reported in the [`TransportRecoveryReport`] rather than returned as
    /// errors; an `Err` from this method means the local store itself failed.
    pub(crate) async fn drain_private_note_backlog(
        &mut self,
        account_id: AccountId,
    ) -> Result<TransportRecoveryReport> {
        let Some(transport) = self.note_transport.clone() else {
            return Ok(TransportRecoveryReport {
                status: TransportRecoveryStatus::Unavailable,
                imported: 0,
                retryable: false,
                reason: Some(
                    "note transport is not configured for this client; \
                     set a transport endpoint to relay private notes"
                        .to_string(),
                ),
                skipped_invalid: 0,
                skipped_unscreenable: 0,
            });
        };

        let tags = self.account_note_tags(account_id).await?;
        if tags.is_empty() {
            return Ok(TransportRecoveryReport {
                status: TransportRecoveryStatus::Failed,
                imported: 0,
                retryable: false,
                reason: Some(format!(
                    "account {account_id} has no tracked note tag; load the account before \
                     draining"
                )),
                skipped_invalid: 0,
                skipped_unscreenable: 0,
            });
        }

        let before = self.input_note_count().await?;
        let mut skipped = Skipped::default();
        // Screening reads the store's chain state, which a fresh store lacks.
        let mut stop = self
            .miden_client
            .ensure_genesis_in_place()
            .await
            .err()
            .map(DrainStop::from);
        if stop.is_none() {
            for group in tags.chunks(MidenSdkClient::MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST) {
                if let Err(err) = self
                    .drain_tag_group(&transport, account_id, group, &mut skipped)
                    .await
                {
                    stop = Some(err);
                    break;
                }
            }
        }
        // A drain never removes records (and `&mut self` excludes concurrent
        // client operations), so the length delta is the count of newly
        // imported records, including pages imported before a failure.
        let imported = self.input_note_count().await?.saturating_sub(before);

        let (status, retryable, reason) = match stop {
            None => (TransportRecoveryStatus::Completed, false, None),
            Some(DrainStop::PageLimit) => (
                TransportRecoveryStatus::Failed,
                false,
                Some(format!(
                    "transport still had pages after {MAX_PAGES_PER_TAG_GROUP} pages for one tag \
                     group; a rerun restarts from the oldest note and stops at the same limit"
                )),
            ),
            Some(DrainStop::InvalidPage) => (
                TransportRecoveryStatus::Failed,
                true,
                Some("transport returned a page with invalid pagination progress".to_string()),
            ),
            // A broken local store is an environment failure, not a transport
            // outcome: the whole recovery flow needs to know, so it
            // propagates instead of being folded into the report.
            Some(DrainStop::Client(err @ ClientError::StoreError(_))) => {
                return Err(MultisigError::miden_client_with_context(
                    "local store failed during the transport drain",
                    err,
                ));
            }
            Some(DrainStop::Client(err)) => {
                let (status, retryable) = classify_drain_failure(&err);
                // `Unavailable` promises "nothing was imported".
                let status = if imported > 0 && status == TransportRecoveryStatus::Unavailable {
                    TransportRecoveryStatus::Failed
                } else {
                    status
                };
                (status, retryable, Some(error_chain(&err)))
            }
        };
        Ok(TransportRecoveryReport {
            status,
            imported,
            retryable,
            reason,
            skipped_invalid: skipped.invalid,
            skipped_unscreenable: skipped.unscreenable,
        })
    }

    async fn account_note_tags(&mut self, account_id: AccountId) -> Result<Vec<NoteTag>> {
        let records =
            self.miden_client.get_note_tags().await.map_err(|e| {
                MultisigError::miden_client_with_context("failed to list note tags", e)
            })?;
        let tags: BTreeSet<NoteTag> = records
            .into_iter()
            .filter(
                |record| matches!(record.source, NoteTagSource::Account(id) if id == account_id),
            )
            .map(|record| record.tag)
            .collect();
        Ok(tags.into_iter().collect())
    }

    // The Err variant carries upstream's `ClientError`.
    #[allow(clippy::result_large_err)]
    async fn drain_tag_group(
        &mut self,
        transport: &Arc<dyn NoteTransportClient>,
        account_id: AccountId,
        tags: &[NoteTag],
        skipped: &mut Skipped,
    ) -> std::result::Result<(), DrainStop> {
        let mut cursor = NoteTransportCursor::init();
        for _ in 0..MAX_PAGES_PER_TAG_GROUP {
            let page = transport
                .fetch_notes_page(tags, cursor)
                .await
                .map_err(ClientError::from)?;
            if !made_progress(cursor, page.cursor, page.notes.is_empty(), page.has_more) {
                return Err(DrainStop::InvalidPage);
            }
            let notes = page
                .notes
                .iter()
                .filter_map(|info| {
                    let note = rejoin_requested_note(info, tags);
                    if note.is_none() {
                        skipped.invalid += 1;
                    }
                    note
                })
                .collect();
            skipped.unscreenable += self.import_page_for_account(account_id, notes).await?;
            if !page.has_more {
                return Ok(());
            }
            cursor = page.cursor;
        }
        Err(DrainStop::PageLimit)
    }

    /// Returns how many notes were skipped because screening them failed.
    #[allow(clippy::result_large_err)]
    async fn import_page_for_account(
        &mut self,
        account_id: AccountId,
        mut notes: Vec<(NoteId, Note, Option<BlockNumber>)>,
    ) -> std::result::Result<usize, ClientError> {
        if notes.is_empty() {
            return Ok(0);
        }

        let commitments = notes
            .iter()
            .map(|(_, note, _)| note.details_commitment())
            .collect();
        let records: BTreeMap<_, _> = self
            .miden_client
            .get_input_notes(NoteFilter::DetailsCommitments(commitments))
            .await?
            .into_iter()
            .map(|record| (record.details_commitment(), record))
            .collect();
        // The import rejects a page holding a note being consumed locally, whatever its id.
        notes.retain(
            |(id, note, _)| match records.get(&note.details_commitment()) {
                None => true,
                Some(record) if record.is_processing() => false,
                Some(record) => {
                    !((record.is_committed() || record.is_consumed()) && record.id() == Some(*id))
                }
            },
        );

        // One at a time, so a note whose check fails is skipped, not the page.
        let screener = self.miden_client.note_screener();
        let mut unscreenable = 0;
        let mut files = Vec::new();
        for (_, note, block_hint) in notes {
            match screener
                .get_batch_consumability_for_account(account_id, std::slice::from_ref(&note))
                .await
            {
                Ok(consumable) if consumable.contains_key(&note.id()) => {
                    let tag = note.metadata().tag();
                    files.push(NoteFile::ExpectedNote {
                        details: note.into(),
                        // Scan from genesis so a late recipient still finds an old commitment.
                        sync_hint: NoteSyncHint::new(
                            block_hint.unwrap_or(BlockNumber::GENESIS),
                            tag,
                        ),
                    });
                }
                Ok(_) => {}
                Err(NoteScreenerError::NoteCheckerError(_)) => unscreenable += 1,
                Err(NoteScreenerError::StoreError(err)) => {
                    return Err(ClientError::StoreError(err));
                }
                Err(err) => return Err(err.into()),
            }
        }
        if !files.is_empty() {
            self.miden_client.import_notes(&files).await?;
        }
        Ok(unscreenable)
    }

    /// Number of input note records currently in the local store.
    async fn input_note_count(&mut self) -> Result<usize> {
        let records = self
            .miden_client
            .get_input_notes(NoteFilter::All)
            .await
            .map_err(|e| {
                MultisigError::miden_client_with_context("failed to list input notes", e)
            })?;
        Ok(records.len())
    }
}

/// Mirrors miden-client's page validation: a page with notes must advance the cursor.
fn made_progress(
    request: NoteTransportCursor,
    page: NoteTransportCursor,
    page_is_empty: bool,
    has_more: bool,
) -> bool {
    let Some((nonce, sequence)) = page.parts() else {
        return false;
    };
    if has_more && page_is_empty || (!page_is_empty && sequence == 0) {
        return false;
    }
    match request.parts() {
        Some((request_nonce, request_sequence)) if request_nonce == nonce => {
            sequence > request_sequence || (page_is_empty && sequence == request_sequence)
        }
        _ => true,
    }
}

/// Rebuilds a transport note like miden-client, or `None` when malformed or of an
/// unrequested tag. The transport carries no attachments.
fn rejoin_requested_note(
    info: &NoteInfo,
    tags: &[NoteTag],
) -> Option<(NoteId, Note, Option<BlockNumber>)> {
    let details = NoteDetails::read_from_bytes(&info.details_bytes).ok()?;
    if info.header.details_commitment() != details.commitment()
        || !tags.contains(&info.header.metadata().tag())
    {
        return None;
    }
    let note = Note::new(
        details.assets().clone(),
        *info.header.metadata().partial_metadata(),
        details.recipient().clone(),
    );
    Some((info.header.id(), note, info.block_hint))
}

/// Maps a transport-drain failure onto a report
/// class: `(status, retryable)`.
fn classify_drain_failure(err: &ClientError) -> (TransportRecoveryStatus, bool) {
    match err {
        // The match is deliberately exhaustive so a new upstream variant is a
        // compile error here rather than a silently wrong classification.
        ClientError::NoteTransportError(transport_err) => match transport_err {
            // No transport configured — retrying cannot help until the client
            // is rebuilt with an endpoint.
            NoteTransportError::Disabled => (TransportRecoveryStatus::Unavailable, false),
            // `Connection` wraps endpoint parsing, TLS configuration, and
            // actual connect failures indiscriminately; the shared classifier
            // inspects the cause chain to tell a retry-worthy dropped
            // connection from a permanently misconfigured endpoint.
            NoteTransportError::Connection(_) => (
                TransportRecoveryStatus::Unavailable,
                is_transient_note_transport_error(transport_err),
            ),
            // The transport answered with an error — worth retrying once the
            // service recovers.
            NoteTransportError::Network(_) => (TransportRecoveryStatus::Unavailable, true),
            // No longer produced since miden-client 0.17.1; matched for exhaustiveness.
            #[allow(deprecated)]
            NoteTransportError::PaginationDidNotTerminate(_) => {
                (TransportRecoveryStatus::Failed, true)
            }
            // Undecodable or inconsistent payloads: a rerun would hit the same
            // bytes again.
            NoteTransportError::Deserialization(_)
            | NoteTransportError::NoteDetailsMismatch { .. }
            | NoteTransportError::InvalidFetchedNote(_)
            | NoteTransportError::UnrequestedTag(_) => (TransportRecoveryStatus::Failed, false),
        },
        // Each fetched batch is imported through the node (inclusion-proof
        // lookup), so a node RPC failure interrupts the drain mid-way; the
        // shared gRPC classifier decides whether a rerun can help.
        ClientError::RpcError(rpc_err) => (
            TransportRecoveryStatus::Failed,
            is_transient_rpc_error(rpc_err),
        ),
        _ => (TransportRecoveryStatus::Failed, false),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use miden_client::ClientError;
    use miden_client::Serializable;
    use miden_client::note_transport::{NoteTransportClient, NoteTransportError};
    use miden_client::testing::note_transport::{MockNoteTransportApi, MockNoteTransportNode};
    use miden_protocol::note::{Note, NoteDetails, NoteTag, NoteType};
    use miden_tx::utils::sync::RwLock;

    use super::*;
    use crate::client::test_support::{
        add_to_transport, mock_transport, offline_client, p2id_note_for, test_wallet,
    };

    // ---------------------------------------------------------------------
    // classification
    // ---------------------------------------------------------------------

    fn transport_error(err: NoteTransportError) -> ClientError {
        ClientError::NoteTransportError(err)
    }

    #[test]
    fn disabled_transport_is_unavailable_and_not_retryable() {
        let (status, retryable) =
            classify_drain_failure(&transport_error(NoteTransportError::Disabled));
        assert_eq!(status, TransportRecoveryStatus::Unavailable);
        assert!(!retryable);
    }

    #[test]
    fn unreachable_transport_is_unavailable_and_retryable() {
        let connection = NoteTransportError::Connection(Box::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        )));
        let (status, retryable) = classify_drain_failure(&transport_error(connection));
        assert_eq!(status, TransportRecoveryStatus::Unavailable);
        assert!(retryable);

        let network = NoteTransportError::Network("transport 503".to_string());
        let (status, retryable) = classify_drain_failure(&transport_error(network));
        assert_eq!(status, TransportRecoveryStatus::Unavailable);
        assert!(retryable);
    }

    /// `Connection` also wraps endpoint-parse/TLS misconfiguration; the
    /// shared cause-chain classifier marks those permanent so a recovery
    /// flow does not loop retrying a client that can never connect.
    #[test]
    fn misconfigured_transport_endpoint_is_unavailable_and_not_retryable() {
        let connection = NoteTransportError::Connection(Box::new(std::io::Error::other(
            "invalid uri: missing scheme",
        )));
        let (status, retryable) = classify_drain_failure(&transport_error(connection));
        assert_eq!(status, TransportRecoveryStatus::Unavailable);
        assert!(!retryable);
    }

    #[test]
    fn node_rpc_failure_mid_drain_is_a_retryable_failure() {
        let err: ClientError = miden_client::rpc::RpcError::RequestError {
            endpoint: miden_client::rpc::RpcEndpoint::SyncChainMmr,
            error_kind: miden_client::rpc::GrpcError::Unavailable,
            endpoint_error: None,
            source: None,
        }
        .into();
        let (status, retryable) = classify_drain_failure(&err);
        assert_eq!(status, TransportRecoveryStatus::Failed);
        assert!(retryable);
    }

    /// The shared gRPC classifier decides retryability for node failures:
    /// a permanent status (bad request, wrong node version) must not tell
    /// callers to rerun the drain.
    #[test]
    fn permanent_node_rpc_failure_mid_drain_is_not_retryable() {
        let err: ClientError = miden_client::rpc::RpcError::RequestError {
            endpoint: miden_client::rpc::RpcEndpoint::SyncChainMmr,
            error_kind: miden_client::rpc::GrpcError::InvalidArgument,
            endpoint_error: None,
            source: None,
        }
        .into();
        let (status, retryable) = classify_drain_failure(&err);
        assert_eq!(status, TransportRecoveryStatus::Failed);
        assert!(!retryable);
    }

    #[test]
    fn other_client_errors_are_permanent_failures() {
        let (status, retryable) = classify_drain_failure(&ClientError::AddNewAccountWithoutSeed);
        assert_eq!(status, TransportRecoveryStatus::Failed);
        assert!(!retryable);
    }

    // ---------------------------------------------------------------------
    // offline behavioral tests (mock chain + mock transport)
    // ---------------------------------------------------------------------

    /// A distinct (per `seed`) private P2ID note addressed at `target`.
    fn private_note_for(target: &miden_protocol::account::Account, seed: u32) -> Note {
        p2id_note_for(target, seed, NoteType::Private)
    }

    #[tokio::test]
    async fn drain_reports_unavailable_when_transport_is_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        let mut client = offline_client(dir.path(), None).await;

        let report = client
            .drain_private_note_backlog(test_wallet(1).id())
            .await
            .expect("disabled transport is reported, not thrown");

        assert_eq!(report.status, TransportRecoveryStatus::Unavailable);
        assert_eq!(report.imported, 0);
        assert!(!report.retryable);
        assert!(report.reason.unwrap().contains("not configured"));
    }

    #[tokio::test]
    async fn drain_recovers_the_backlog_for_a_tracked_account_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let (node, api) = mock_transport();
        let mut client = offline_client(dir.path(), Some(api)).await;

        // The recovered account is in the store (as after pull_account), so
        // its standard note tag is tracked.
        let account = test_wallet(1);
        client.add_or_update_account(&account, false).await.unwrap();
        add_to_transport(&node, private_note_for(&account, 1));

        let report = client
            .drain_private_note_backlog(account.id())
            .await
            .unwrap();
        assert_eq!(report.status, TransportRecoveryStatus::Completed);
        assert_eq!(report.imported, 1);
        assert!(!report.retryable);
        assert_eq!(report.reason, None);

        // Idempotence: draining again imports nothing new.
        let report = client
            .drain_private_note_backlog(account.id())
            .await
            .unwrap();
        assert_eq!(report.status, TransportRecoveryStatus::Completed);
        assert_eq!(report.imported, 0);
    }

    #[tokio::test]
    async fn drain_of_an_account_not_in_the_store_fails_visibly() {
        let dir = tempfile::tempdir().unwrap();
        let (node, api) = mock_transport();
        let mut client = offline_client(dir.path(), Some(api)).await;

        let account = test_wallet(2);
        add_to_transport(&node, private_note_for(&account, 1));

        let report = client
            .drain_private_note_backlog(account.id())
            .await
            .unwrap();
        assert_eq!(report.status, TransportRecoveryStatus::Failed);
        assert!(!report.retryable);
        assert_eq!(report.imported, 0);
        assert!(report.reason.unwrap().contains("no tracked note tag"));
    }

    #[tokio::test]
    async fn drain_skips_foreign_notes_on_a_shared_tag() {
        let dir = tempfile::tempdir().unwrap();
        let (node, api) = mock_transport();
        let mut client = offline_client(dir.path(), Some(api)).await;

        let recovered = test_wallet(10);
        let other = test_wallet(11);
        client
            .add_or_update_account(&recovered, false)
            .await
            .unwrap();
        client.add_or_update_account(&other, false).await.unwrap();

        let own = private_note_for(&recovered, 1);
        let addressed_to_other = private_note_for(&other, 2);
        let shared_tag = own.metadata().tag();
        let foreign = Note::new(
            addressed_to_other.assets().clone(),
            addressed_to_other
                .metadata()
                .partial_metadata()
                .with_tag(shared_tag),
            addressed_to_other.recipient().clone(),
        );
        add_to_transport(&node, own.clone());
        add_to_transport(&node, foreign.clone());

        let report = client
            .drain_private_note_backlog(recovered.id())
            .await
            .unwrap();
        assert_eq!(report.status, TransportRecoveryStatus::Completed);
        assert_eq!(report.imported, 1);

        let records = client
            .miden_client
            .get_input_notes(NoteFilter::All)
            .await
            .unwrap();
        assert!(
            records
                .iter()
                .any(|r| r.details_commitment() == own.details_commitment())
        );
        assert!(
            !records
                .iter()
                .any(|r| r.details_commitment() == foreign.details_commitment())
        );
    }

    #[tokio::test]
    async fn drain_skips_and_counts_malformed_notes() {
        let dir = tempfile::tempdir().unwrap();
        let (node, api) = mock_transport();
        let mut client = offline_client(dir.path(), Some(api)).await;

        let account = test_wallet(12);
        client.add_or_update_account(&account, false).await.unwrap();
        let good = private_note_for(&account, 1);
        let other = private_note_for(&account, 2);
        node.write()
            .add_note(*other.header(), NoteDetails::from(good.clone()).to_bytes());
        add_to_transport(&node, good);

        let report = client
            .drain_private_note_backlog(account.id())
            .await
            .unwrap();
        assert_eq!(report.status, TransportRecoveryStatus::Completed);
        assert_eq!(report.imported, 1);
        assert_eq!(report.skipped_invalid, 1);
    }

    /// More than the 32 pages one client sync reads.
    #[tokio::test]
    async fn drain_pages_the_whole_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let node = Arc::new(RwLock::new(MockNoteTransportNode::with_max_batch(1)));
        let api: Arc<dyn NoteTransportClient> = Arc::new(MockNoteTransportApi::new(node.clone()));
        let mut client = offline_client(dir.path(), Some(api)).await;

        let account = test_wallet(8);
        client.add_or_update_account(&account, false).await.unwrap();
        let count = 40;
        for seed in 1..=count {
            add_to_transport(&node, private_note_for(&account, seed));
        }

        let report = client
            .drain_private_note_backlog(account.id())
            .await
            .unwrap();
        assert_eq!(report.status, TransportRecoveryStatus::Completed);
        assert_eq!(report.imported, count as usize);
    }

    /// Transport that serves a limited number of successful fetches, then
    /// fails with a connection-shaped error — a connection dropped mid-drain.
    struct InterruptibleTransport {
        inner: MockNoteTransportApi,
        fetches_before_failure: std::sync::atomic::AtomicUsize,
        failed: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl NoteTransportClient for InterruptibleTransport {
        async fn send_note_with_proof(
            &self,
            note: miden_client::note_transport::TransportNote,
            inclusion_proof: miden_protocol::note::NoteInclusionProof,
        ) -> std::result::Result<(), NoteTransportError> {
            self.inner.send_note_with_proof(note, &inclusion_proof);
            Ok(())
        }

        async fn fetch_notes(
            &self,
            tags: &[NoteTag],
            cursor: miden_client::note_transport::NoteTransportCursor,
        ) -> std::result::Result<
            (
                Vec<miden_client::note_transport::NoteInfo>,
                miden_client::note_transport::NoteTransportCursor,
            ),
            NoteTransportError,
        > {
            let allowed = self
                .fetches_before_failure
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                )
                .is_ok();
            if !allowed && !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(NoteTransportError::Network(
                    "connection dropped mid-drain".to_string(),
                ));
            }
            Ok(self.inner.fetch_notes(tags, cursor))
        }
    }

    #[tokio::test]
    async fn interrupted_drain_keeps_fetched_pages_and_is_retryable() {
        let dir = tempfile::tempdir().unwrap();
        // Cap each response at one note so the two-note backlog needs two
        // fetches; the transport dies after the first.
        let node = Arc::new(RwLock::new(MockNoteTransportNode::with_max_batch(1)));
        let api: Arc<dyn NoteTransportClient> = Arc::new(InterruptibleTransport {
            inner: MockNoteTransportApi::new(node.clone()),
            fetches_before_failure: std::sync::atomic::AtomicUsize::new(1),
            failed: std::sync::atomic::AtomicBool::new(false),
        });
        let mut client = offline_client(dir.path(), Some(api)).await;

        let account = test_wallet(6);
        client.add_or_update_account(&account, false).await.unwrap();
        add_to_transport(&node, private_note_for(&account, 1));
        add_to_transport(&node, private_note_for(&account, 2));

        let report = client
            .drain_private_note_backlog(account.id())
            .await
            .unwrap();

        assert_eq!(report.status, TransportRecoveryStatus::Failed);
        assert!(report.retryable);
        assert_eq!(report.imported, 1);
        assert!(report.reason.unwrap().contains("connection dropped"));

        let report = client
            .drain_private_note_backlog(account.id())
            .await
            .unwrap();
        assert_eq!(report.status, TransportRecoveryStatus::Completed);
        assert_eq!(report.imported, 1);
    }

    struct CountingTransport {
        inner: MockNoteTransportApi,
        requests: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl NoteTransportClient for CountingTransport {
        async fn send_note_with_proof(
            &self,
            note: miden_client::note_transport::TransportNote,
            inclusion_proof: miden_protocol::note::NoteInclusionProof,
        ) -> std::result::Result<(), NoteTransportError> {
            self.inner.send_note_with_proof(note, &inclusion_proof);
            Ok(())
        }

        async fn fetch_notes(
            &self,
            tags: &[NoteTag],
            cursor: NoteTransportCursor,
        ) -> std::result::Result<
            (
                Vec<miden_client::note_transport::NoteInfo>,
                NoteTransportCursor,
            ),
            NoteTransportError,
        > {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.inner.fetch_notes(tags, cursor))
        }

        async fn fetch_notes_page(
            &self,
            tags: &[NoteTag],
            cursor: NoteTransportCursor,
        ) -> std::result::Result<miden_client::note_transport::NoteTransportPage, NoteTransportError>
        {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.fetch_notes_page(tags, cursor).await
        }
    }

    /// The retry wrapper must keep `has_more`, or the drain asks for an extra empty page.
    #[tokio::test]
    async fn drain_through_the_retry_wrapper_stops_on_the_last_page() {
        let dir = tempfile::tempdir().unwrap();
        let node = Arc::new(RwLock::new(MockNoteTransportNode::with_max_batch(1)));
        let counting = Arc::new(CountingTransport {
            inner: MockNoteTransportApi::new(node.clone()),
            requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let api: Arc<dyn NoteTransportClient> =
            Arc::new(crate::rpc::RetryingNoteTransportClient::new(
                counting.clone() as Arc<dyn NoteTransportClient>,
                &crate::rpc::RpcRetryPolicy::new(2),
            ));
        let mut client = offline_client(dir.path(), Some(api)).await;

        let account = test_wallet(13);
        client.add_or_update_account(&account, false).await.unwrap();
        add_to_transport(&node, private_note_for(&account, 1));
        add_to_transport(&node, private_note_for(&account, 2));

        let report = client
            .drain_private_note_backlog(account.id())
            .await
            .unwrap();
        assert_eq!(report.status, TransportRecoveryStatus::Completed);
        assert_eq!(report.imported, 2);
        assert_eq!(
            counting.requests.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    #[test]
    fn made_progress_accepts_forward_pages_and_rejects_stalls() {
        let at = |seq| NoteTransportCursor::from_parts(1, seq);
        let start = NoteTransportCursor::init();
        assert!(made_progress(start, at(3), false, true));
        assert!(made_progress(at(3), at(3), true, false));
        assert!(!made_progress(start, start, true, false));
        assert!(!made_progress(at(3), at(4), true, true));
        assert!(!made_progress(at(3), at(3), false, true));
        assert!(!made_progress(at(3), at(2), true, false));
        assert!(!made_progress(start, at(0), false, false));
        // Another transport database restarts the sequence.
        assert!(made_progress(
            at(9),
            NoteTransportCursor::from_parts(2, 1),
            false,
            false
        ));
    }

    // ---------------------------------------------------------------------
    // load/tag regression suite: the store-side behavior pull_account relies
    // on, which gates whether a drain sees the recovered account at all.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn adding_an_account_tracks_its_standard_note_tag() {
        let dir = tempfile::tempdir().unwrap();
        let mut client = offline_client(dir.path(), None).await;

        let account = test_wallet(4);
        client.add_or_update_account(&account, false).await.unwrap();

        let expected = NoteTag::with_account_target(account.id());
        let tags = client.miden_client.get_note_tags().await.unwrap();
        assert!(
            tags.iter().any(|record| record.tag == expected),
            "the recovered account's standard note tag must be tracked after insert"
        );
    }

    #[tokio::test]
    async fn re_adding_an_existing_account_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut client = offline_client(dir.path(), None).await;

        let account = test_wallet(5);
        client.add_or_update_account(&account, false).await.unwrap();
        // Reload path: pull_account calls add_or_update_account(_, true) on
        // an account that is already present.
        client.add_or_update_account(&account, true).await.unwrap();

        let expected = NoteTag::with_account_target(account.id());
        let tags = client.miden_client.get_note_tags().await.unwrap();
        assert_eq!(
            tags.iter().filter(|record| record.tag == expected).count(),
            1,
            "reload must not duplicate or drop the account's note tag"
        );
        assert!(
            client
                .miden_client
                .get_account(account.id())
                .await
                .unwrap()
                .is_some()
        );
    }
}
