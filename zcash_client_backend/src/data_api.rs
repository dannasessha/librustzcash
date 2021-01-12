use std::cmp;
use std::collections::HashMap;
use std::fmt::Debug;

use zcash_primitives::{
    block::BlockHash,
    consensus::{self, BlockHeight},
    merkle_tree::{CommitmentTree, IncrementalWitness},
    note_encryption::Memo,
    primitives::{Note, Nullifier, PaymentAddress},
    sapling::Node,
    transaction::{components::Amount, Transaction, TxId},
    zip32::ExtendedFullViewingKey,
};

use crate::{
    address::RecipientAddress,
    data_api::wallet::ANCHOR_OFFSET,
    decrypt::DecryptedOutput,
    proto::compact_formats::CompactBlock,
    wallet::{AccountId, SpendableNote, WalletShieldedOutput, WalletTx},
};

pub mod chain;
pub mod error;
pub mod wallet;

/// Read-only operations require for light wallet functions.
///
/// This trait defines the read-only portion of the storage
/// interface atop which higher-level wallet operations are
/// implemented. It serves to allow wallet functions to be
/// abstracted away from any particular data storage substrate.
pub trait WalletRead {
    /// The type of errors produced by a wallet backend.
    type Error;

    /// Backend-specific note identifier.
    ///
    /// For example, this might be a database identifier type
    /// or a UUID.
    type NoteRef: Copy + Debug;

    /// Backend-specific transaction identifier.
    ///
    /// For example, this might be a database identifier type
    /// or a TxId if the backend is able to support that type
    /// directly.
    type TxRef: Copy + Debug;

    /// Returns the minimum and maximum block heights for stored blocks.
    fn block_height_extrema(&self) -> Result<Option<(BlockHeight, BlockHeight)>, Self::Error>;

    /// Returns the default target height and anchor height, given the
    /// range of block heights that the backend knows about.
    fn get_target_and_anchor_heights(
        &self,
    ) -> Result<Option<(BlockHeight, BlockHeight)>, Self::Error> {
        self.block_height_extrema().map(|heights| {
            heights.map(|(min_height, max_height)| {
                let target_height = max_height + 1;

                // Select an anchor ANCHOR_OFFSET back from the target block,
                // unless that would be before the earliest block we have.
                let anchor_height = BlockHeight::from(cmp::max(
                    u32::from(target_height).saturating_sub(ANCHOR_OFFSET),
                    u32::from(min_height),
                ));

                (target_height, anchor_height)
            })
        })
    }

    /// Returns the block hash for the block at the given height
    fn get_block_hash(&self, block_height: BlockHeight) -> Result<Option<BlockHash>, Self::Error>;

    /// Returns the block hash for the block at the maximum height known
    /// in stored data.
    fn get_max_height_hash(&self) -> Result<Option<(BlockHeight, BlockHash)>, Self::Error> {
        self.block_height_extrema()
            .and_then(|extrema_opt| {
                extrema_opt
                    .map(|(_, max_height)| {
                        self.get_block_hash(max_height)
                            .map(|hash_opt| hash_opt.map(move |hash| (max_height, hash)))
                    })
                    .transpose()
            })
            .map(|oo| oo.flatten())
    }

    /// Returns the block height in which the specified transaction was mined.
    fn get_tx_height(&self, txid: TxId) -> Result<Option<BlockHeight>, Self::Error>;

    /// Returns the payment address for the specified account, if the account
    /// identifier specified refers to a valid account for this wallet.
    fn get_address<P: consensus::Parameters>(
        &self,
        params: &P,
        account: AccountId,
    ) -> Result<Option<PaymentAddress>, Self::Error>;

    /// Returns all extended full viewing keys known about by this wallet
    // TODO: Should this also take an AccountId as argument?
    fn get_extended_full_viewing_keys<P: consensus::Parameters>(
        &self,
        params: &P,
    ) -> Result<HashMap<AccountId, ExtendedFullViewingKey>, Self::Error>;

    /// Checks whether the specified extended full viewing key is 
    /// associated with the account.
    fn is_valid_account_extfvk<P: consensus::Parameters>(
        &self,
        params: &P,
        account: AccountId,
        extfvk: &ExtendedFullViewingKey,
    ) -> Result<bool, Self::Error>;

    /// Returns the wallet balance for the specified account.
    ///
    /// This balance amount is the raw balance of all transactions in known
    /// mined blocks, irrespective of confirmation depth.
    // TODO: Do we actually need this? You can always get the "verified"
    // balance from the current chain tip.
    fn get_balance(&self, account: AccountId) -> Result<Amount, Self::Error>;

    /// Returns the wallet balance for an account as of the specified block
    /// height. and
    ///
    /// This may be used to obtain a balance that ignores notes that have been
    /// received so recently that they are not yet deemed spendable.
    fn get_verified_balance(
        &self,
        account: AccountId,
        anchor_height: BlockHeight,
    ) -> Result<Amount, Self::Error>;

    /// Returns the memo for a received note, if it is known and a valid UTF-8 string.
    fn get_received_memo_as_utf8(
        &self,
        id_note: Self::NoteRef,
    ) -> Result<Option<String>, Self::Error>;

    /// Returns the memo for a sent note, if it is known and a valid UTF-8 string.
    fn get_sent_memo_as_utf8(&self, id_note: Self::NoteRef) -> Result<Option<String>, Self::Error>;

    /// Returns the note commitment tree at the specified block height.
    fn get_commitment_tree(
        &self,
        block_height: BlockHeight,
    ) -> Result<Option<CommitmentTree<Node>>, Self::Error>;

    /// Returns the incremental witnesses as of the specified block height.
    fn get_witnesses(
        &self,
        block_height: BlockHeight,
    ) -> Result<Vec<(Self::NoteRef, IncrementalWitness<Node>)>, Self::Error>;

    /// Returns the unspent nullifiers, along with the account identifiers
    /// with which they are associated.
    fn get_nullifiers(&self) -> Result<Vec<(Nullifier, AccountId)>, Self::Error>;

    /// Returns a list of spendable notes sufficient to cover the specified
    /// target value, if possible.
    fn select_spendable_notes(
        &self,
        account: AccountId,
        target_value: Amount,
        anchor_height: BlockHeight,
    ) -> Result<Vec<SpendableNote>, Self::Error>;
}

/// This trait encapsulate the write capabilities required to update stored
/// wallet data.
pub trait WalletWrite: WalletRead {
    /// Perform one or more write operations of this trait transactionally.
    /// Implementations of this method must ensure that all mutations to the
    /// state of the data store made by the provided closure must be performed
    /// atomically and modifications to state must be automatically rolled back
    /// if the provided closure returns an error.
    fn transactionally<F, A>(&mut self, f: F) -> Result<A, Self::Error>
    where
        F: FnOnce(&mut Self) -> Result<A, Self::Error>;

    /// Add the data for a block to the data store.
    fn insert_block(
        &mut self,
        block_height: BlockHeight,
        block_hash: BlockHash,
        block_time: u32,
        commitment_tree: &CommitmentTree<Node>,
    ) -> Result<(), Self::Error>;

    /// This method assumes that the state of the underlying data store is
    /// consistent up to a particular block height. Since it is possible that
    /// a chain reorg might invalidate some stored state, this method must be
    /// implemented in order to allow users of this API to "reset" the data store
    /// to correctly represent chainstate as of a specified block height.
    fn rewind_to_height<P: consensus::Parameters>(
        &mut self,
        parameters: &P,
        block_height: BlockHeight,
    ) -> Result<(), Self::Error>;

    /// Add wallet-relevant metadata for a specific transaction to the data
    /// store.
    fn put_tx_meta(
        &mut self,
        tx: &WalletTx,
        height: BlockHeight,
    ) -> Result<Self::TxRef, Self::Error>;

    /// Add a full transaction contents to the data store.
    fn put_tx_data(
        &mut self,
        tx: &Transaction,
        created_at: Option<time::OffsetDateTime>,
    ) -> Result<Self::TxRef, Self::Error>;

    /// Mark the specified transaction as spent and record the nullifier.
    fn mark_spent(&mut self, tx_ref: Self::TxRef, nf: &Nullifier) -> Result<(), Self::Error>;

    /// Record a note as having been received, along with its nullifier and the transaction 
    /// within which the note was created.
    fn put_received_note<T: ShieldedOutput>(
        &mut self,
        output: &T,
        nf: &Option<Nullifier>,
        tx_ref: Self::TxRef,
    ) -> Result<Self::NoteRef, Self::Error>;

    fn insert_witness(
        &mut self,
        note_id: Self::NoteRef,
        witness: &IncrementalWitness<Node>,
        height: BlockHeight,
    ) -> Result<(), Self::Error>;

    fn prune_witnesses(&mut self, from_height: BlockHeight) -> Result<(), Self::Error>;

    fn update_expired_notes(&mut self, from_height: BlockHeight) -> Result<(), Self::Error>;

    fn put_sent_note<P: consensus::Parameters>(
        &mut self,
        params: &P,
        output: &DecryptedOutput,
        tx_ref: Self::TxRef,
    ) -> Result<(), Self::Error>;

    fn insert_sent_note<P: consensus::Parameters>(
        &mut self,
        params: &P,
        tx_ref: Self::TxRef,
        output_index: usize,
        account: AccountId,
        to: &RecipientAddress,
        value: Amount,
        memo: Option<Memo>,
    ) -> Result<(), Self::Error>;
}

pub trait BlockSource {
    type Error;

    fn init_cache(&self) -> Result<(), Self::Error>;

    fn with_blocks<F>(
        &self,
        from_height: BlockHeight,
        limit: Option<u32>,
        with_row: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(CompactBlock) -> Result<(), Self::Error>;
}

pub trait ShieldedOutput {
    fn index(&self) -> usize;
    fn account(&self) -> AccountId;
    fn to(&self) -> &PaymentAddress;
    fn note(&self) -> &Note;
    fn memo(&self) -> Option<&Memo>;
    fn is_change(&self) -> Option<bool>;
}

impl ShieldedOutput for WalletShieldedOutput {
    fn index(&self) -> usize {
        self.index
    }
    fn account(&self) -> AccountId {
        self.account
    }
    fn to(&self) -> &PaymentAddress {
        &self.to
    }
    fn note(&self) -> &Note {
        &self.note
    }
    fn memo(&self) -> Option<&Memo> {
        None
    }
    fn is_change(&self) -> Option<bool> {
        Some(self.is_change)
    }
}

impl ShieldedOutput for DecryptedOutput {
    fn index(&self) -> usize {
        self.index
    }
    fn account(&self) -> AccountId {
        self.account
    }
    fn to(&self) -> &PaymentAddress {
        &self.to
    }
    fn note(&self) -> &Note {
        &self.note
    }
    fn memo(&self) -> Option<&Memo> {
        Some(&self.memo)
    }
    fn is_change(&self) -> Option<bool> {
        None
    }
}
