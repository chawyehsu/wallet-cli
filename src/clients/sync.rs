// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

use std::mem::size_of;

use dusk_bytes::{DeserializableSlice, Serializable};
use dusk_plonk::prelude::BlsScalar;
use dusk_wallet_core::Store;
use futures::StreamExt;
use phoenix_core::transaction::{ArchivedTreeLeaf, TreeLeaf};

use crate::block::Block;
use crate::clients::{Cache, TRANSFER_CONTRACT};
use crate::rusk::RuskHttpClient;
use crate::store::LocalStore;
use crate::{Error, RuskRequest, MAX_ADDRESSES};

const RKYV_TREE_LEAF_SIZE: usize = size_of::<ArchivedTreeLeaf>();

pub(crate) async fn sync_db(
    client: &mut RuskHttpClient,
    store: &LocalStore,
    cache: &Cache,
    status: fn(&str),
) -> Result<(), Error> {
    let addresses: Vec<_> = (0..MAX_ADDRESSES)
        .flat_map(|i| store.retrieve_ssk(i as u64))
        .map(|ssk| {
            let vk = ssk.view_key();
            let psk = vk.public_spend_key();
            (ssk, vk, psk)
        })
        .collect();

    status("Getting cached note position...");

    let last_pos = cache.last_pos()?;
    let pos_to_search = last_pos.map(|p| p + 1).unwrap_or_default();
    let mut last_pos = last_pos.unwrap_or_default();

    status("Fetching fresh notes...");

    let req = rkyv::to_bytes::<_, 8>(&(pos_to_search))
        .map_err(|_| Error::Rkyv)?
        .to_vec();

    let mut stream = client
        .call_raw(
            1,
            TRANSFER_CONTRACT,
            &RuskRequest::new("leaves_from_pos", req),
            true,
        )
        .await?
        .bytes_stream();

    status("Connection established...");

    status("Streaming notes...");

    // This buffer is needed because `.bytes_stream();` introduce additional
    // spliting of chunks according to it's own buffer
    let mut buffer = vec![];

    while let Some(http_chunk) = stream.next().await {
        buffer.extend_from_slice(&http_chunk?);

        let mut leaf_chunk = buffer.chunks_exact(RKYV_TREE_LEAF_SIZE);

        for leaf_bytes in leaf_chunk.by_ref() {
            let TreeLeaf { block_height, note } =
                rkyv::from_bytes(leaf_bytes).map_err(|_| Error::Rkyv)?;

            last_pos = std::cmp::max(last_pos, *note.pos());

            for (ssk, vk, psk) in addresses.iter() {
                if vk.owns(&note) {
                    let note_data = (note, note.gen_nullifier(ssk));
                    cache.insert(psk, block_height, note_data)?;

                    break;
                }
            }
            cache.insert_last_pos(last_pos)?;
        }

        buffer = leaf_chunk.remainder().to_vec();
    }

    // Remove spent nullifiers from live notes
    for (_, _, psk) in addresses {
        let cf_name = format!("{:?}", psk);
        let mut nullifiers = vec![];

        if let Some(cf) = cache.db.cf_handle(&cf_name) {
            let iterator =
                cache.db.iterator_cf(&cf, rocksdb::IteratorMode::Start);

            for i in iterator {
                let (nullifier, _) = i?;
                let nullifier = BlsScalar::from_slice(&nullifier)
                    .expect("key to be a BlsScalar");
                nullifiers.push(nullifier);
            }

            if !nullifiers.is_empty() {
                let spent_cf = format!("spent_{:?}", psk);
                let spent_cf =
                    cache.db.cf_handle(&spent_cf).expect("spent_cf to exists");
                let existing =
                    fetch_existing_nullifiers_remote(client, &nullifiers)
                        .wait()?;
                for n in existing {
                    let key = n.to_bytes();
                    let to_move = cache
                        .db
                        .get_cf(&cf, key)?
                        .expect("Note must exists to be moved");
                    cache.db.put_cf(&spent_cf, key, to_move)?;
                    cache.db.delete_cf(&cf, n.to_bytes())?;
                }
            }
        };
    }
    Ok(())
}

/// Asks the node to return the nullifiers that already exist from the given
/// nullifiers.
pub(crate) async fn fetch_existing_nullifiers_remote(
    client: &RuskHttpClient,
    nullifiers: &[BlsScalar],
) -> Result<Vec<BlsScalar>, Error> {
    if nullifiers.is_empty() {
        return Ok(vec![]);
    }
    let nullifiers = nullifiers.to_vec();
    let data = client
        .contract_query::<_, 1024>(
            TRANSFER_CONTRACT,
            "existing_nullifiers",
            &nullifiers,
        )
        .await?;

    let nullifiers = rkyv::from_bytes(&data).map_err(|_| Error::Rkyv)?;

    Ok(nullifiers)
}
