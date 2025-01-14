use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lru::LruCache;
use parking_lot::Mutex;
use thiserror::Error;

use ckb_hash::blake2b_256;
use ckb_jsonrpc_types as json_types;
use ckb_types::{
    bytes::Bytes,
    core::{BlockView, DepType, HeaderView, TransactionView},
    packed::{Byte32, CellDep, CellOutput, OutPoint, Script, Transaction},
    prelude::*,
    H160,
};

use super::{OffchainCellCollector, OffchainCellDepResolver};
use crate::constants::{
    DAO_OUTPUT_LOC, DAO_TYPE_HASH, MULTISIG_GROUP_OUTPUT_LOC, MULTISIG_OUTPUT_LOC,
    MULTISIG_TYPE_HASH, SIGHASH_GROUP_OUTPUT_LOC, SIGHASH_OUTPUT_LOC, SIGHASH_TYPE_HASH,
};
use crate::rpc::ckb_indexer::{Order, SearchKey, Tip};
use crate::rpc::{CkbRpcClient, IndexerRpcClient};
use crate::traits::{
    CellCollector, CellCollectorError, CellDepResolver, CellQueryOptions, HeaderDepResolver,
    LiveCell, QueryOrder, Signer, SignerError, TransactionDependencyError,
    TransactionDependencyProvider,
};
use crate::types::ScriptId;
use crate::util::{get_max_mature_number, serialize_signature, zeroize_privkey};
use crate::SECP256K1;
use ckb_resource::{
    CODE_HASH_DAO, CODE_HASH_SECP256K1_BLAKE160_MULTISIG_ALL,
    CODE_HASH_SECP256K1_BLAKE160_SIGHASH_ALL,
};

/// Parse Genesis Info errors
#[derive(Error, Debug)]
pub enum ParseGenesisInfoError {
    #[error("invalid block number, expected: 0, got: `{0}`")]
    InvalidBlockNumber(u64),
    #[error("data not found: `{0}`")]
    DataHashNotFound(String),
    #[error("type not found: `{0}`")]
    TypeHashNotFound(String),
}

/// A cell_dep resolver use genesis info resolve system scripts and can register more cell_dep info.
#[derive(Clone)]
pub struct DefaultCellDepResolver {
    offchain: OffchainCellDepResolver,
}
impl DefaultCellDepResolver {
    pub fn from_genesis(
        genesis_block: &BlockView,
    ) -> Result<DefaultCellDepResolver, ParseGenesisInfoError> {
        let header = genesis_block.header();
        if header.number() != 0 {
            return Err(ParseGenesisInfoError::InvalidBlockNumber(header.number()));
        }
        let mut sighash_data_hash = None;
        let mut sighash_type_hash = None;
        let mut multisig_data_hash = None;
        let mut multisig_type_hash = None;
        let mut dao_data_hash = None;
        let mut dao_type_hash = None;
        let out_points = genesis_block
            .transactions()
            .iter()
            .enumerate()
            .map(|(tx_index, tx)| {
                tx.outputs()
                    .into_iter()
                    .zip(tx.outputs_data().into_iter())
                    .enumerate()
                    .map(|(index, (output, data))| {
                        if tx_index == SIGHASH_OUTPUT_LOC.0 && index == SIGHASH_OUTPUT_LOC.1 {
                            sighash_type_hash = output
                                .type_()
                                .to_opt()
                                .map(|script| script.calc_script_hash());
                            let data_hash = CellOutput::calc_data_hash(&data.raw_data());
                            if data_hash != CODE_HASH_SECP256K1_BLAKE160_SIGHASH_ALL.pack() {
                                log::error!(
                                    "System sighash script code hash error! found: {}, expected: {}",
                                    data_hash,
                                    CODE_HASH_SECP256K1_BLAKE160_SIGHASH_ALL,
                                );
                            }
                            sighash_data_hash = Some(data_hash);
                        }
                        if tx_index == MULTISIG_OUTPUT_LOC.0 && index == MULTISIG_OUTPUT_LOC.1 {
                            multisig_type_hash = output
                                .type_()
                                .to_opt()
                                .map(|script| script.calc_script_hash());
                            let data_hash = CellOutput::calc_data_hash(&data.raw_data());
                            if data_hash != CODE_HASH_SECP256K1_BLAKE160_MULTISIG_ALL.pack() {
                                log::error!(
                                    "System multisig script code hash error! found: {}, expected: {}",
                                    data_hash,
                                    CODE_HASH_SECP256K1_BLAKE160_MULTISIG_ALL,
                                );
                            }
                            multisig_data_hash = Some(data_hash);
                        }
                        if tx_index == DAO_OUTPUT_LOC.0 && index == DAO_OUTPUT_LOC.1 {
                            dao_type_hash = output
                                .type_()
                                .to_opt()
                                .map(|script| script.calc_script_hash());
                            let data_hash = CellOutput::calc_data_hash(&data.raw_data());
                            if data_hash != CODE_HASH_DAO.pack() {
                                log::error!(
                                    "System dao script code hash error! found: {}, expected: {}",
                                    data_hash,
                                    CODE_HASH_DAO,
                                );
                            }
                            dao_data_hash = Some(data_hash);
                        }
                        OutPoint::new(tx.hash(), index as u32)
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let sighash_type_hash = sighash_type_hash
            .ok_or_else(|| "No type hash(sighash) found in txs[0][1]".to_owned())
            .map_err(ParseGenesisInfoError::TypeHashNotFound)?;
        let multisig_type_hash = multisig_type_hash
            .ok_or_else(|| "No type hash(multisig) found in txs[0][4]".to_owned())
            .map_err(ParseGenesisInfoError::TypeHashNotFound)?;
        let dao_type_hash = dao_type_hash
            .ok_or_else(|| "No type hash(dao) found in txs[0][2]".to_owned())
            .map_err(ParseGenesisInfoError::TypeHashNotFound)?;

        let sighash_dep = CellDep::new_builder()
            .out_point(out_points[SIGHASH_GROUP_OUTPUT_LOC.0][SIGHASH_GROUP_OUTPUT_LOC.1].clone())
            .dep_type(DepType::DepGroup.into())
            .build();
        let multisig_dep = CellDep::new_builder()
            .out_point(out_points[MULTISIG_GROUP_OUTPUT_LOC.0][MULTISIG_GROUP_OUTPUT_LOC.1].clone())
            .dep_type(DepType::DepGroup.into())
            .build();
        let dao_dep = CellDep::new_builder()
            .out_point(out_points[DAO_OUTPUT_LOC.0][DAO_OUTPUT_LOC.1].clone())
            .build();

        let mut items = HashMap::default();
        items.insert(
            ScriptId::new_type(sighash_type_hash.unpack()),
            (sighash_dep, "Secp256k1 blake160 sighash all".to_string()),
        );
        items.insert(
            ScriptId::new_type(multisig_type_hash.unpack()),
            (multisig_dep, "Secp256k1 blake160 multisig all".to_string()),
        );
        items.insert(
            ScriptId::new_type(dao_type_hash.unpack()),
            (dao_dep, "Nervos DAO".to_string()),
        );
        let offchain = OffchainCellDepResolver { items };
        Ok(DefaultCellDepResolver { offchain })
    }
    pub fn insert(
        &mut self,
        script_id: ScriptId,
        cell_dep: CellDep,
        name: String,
    ) -> Option<(CellDep, String)> {
        self.offchain.items.insert(script_id, (cell_dep, name))
    }
    pub fn remove(&mut self, script_id: &ScriptId) -> Option<(CellDep, String)> {
        self.offchain.items.remove(script_id)
    }
    pub fn contains(&self, script_id: &ScriptId) -> bool {
        self.offchain.items.contains_key(script_id)
    }
    pub fn get(&self, script_id: &ScriptId) -> Option<&(CellDep, String)> {
        self.offchain.items.get(script_id)
    }
    pub fn sighash_dep(&self) -> Option<&(CellDep, String)> {
        self.get(&ScriptId::new_type(SIGHASH_TYPE_HASH))
    }
    pub fn multisig_dep(&self) -> Option<&(CellDep, String)> {
        self.get(&ScriptId::new_type(MULTISIG_TYPE_HASH))
    }
    pub fn dao_dep(&self) -> Option<&(CellDep, String)> {
        self.get(&ScriptId::new_type(DAO_TYPE_HASH))
    }
}

impl CellDepResolver for DefaultCellDepResolver {
    fn resolve(&self, script: &Script) -> Option<CellDep> {
        self.offchain.resolve(script)
    }
}

/// A header_dep resolver use ckb jsonrpc client as backend
pub struct DefaultHeaderDepResolver {
    ckb_client: Arc<Mutex<CkbRpcClient>>,
}
impl DefaultHeaderDepResolver {
    pub fn new(ckb_client: &str) -> DefaultHeaderDepResolver {
        let ckb_client = Arc::new(Mutex::new(CkbRpcClient::new(ckb_client)));
        DefaultHeaderDepResolver { ckb_client }
    }
}
impl HeaderDepResolver for DefaultHeaderDepResolver {
    fn resolve_by_tx(
        &self,
        tx_hash: &Byte32,
    ) -> Result<Option<HeaderView>, Box<dyn std::error::Error>> {
        let mut client = self.ckb_client.lock();
        if let Some(block_hash) = client
            .get_transaction(tx_hash.unpack())
            .map_err(Box::new)?
            .and_then(|tx_with_status| tx_with_status.tx_status.block_hash)
        {
            Ok(client
                .get_header(block_hash)
                .map_err(Box::new)?
                .map(Into::into))
        } else {
            Ok(None)
        }
    }
    fn resolve_by_number(
        &self,
        number: u64,
    ) -> Result<Option<HeaderView>, Box<dyn std::error::Error>> {
        Ok(self
            .ckb_client
            .lock()
            .get_header_by_number(number.into())
            .map_err(Box::new)?
            .map(Into::into))
    }
}

/// A cell collector use ckb-indexer as backend
pub struct DefaultCellCollector {
    indexer_client: IndexerRpcClient,
    ckb_client: CkbRpcClient,
    offchain: OffchainCellCollector,
}

impl DefaultCellCollector {
    pub fn new(indexer_client: &str, ckb_client: &str) -> DefaultCellCollector {
        let indexer_client = IndexerRpcClient::new(indexer_client);
        let ckb_client = CkbRpcClient::new(ckb_client);
        DefaultCellCollector {
            indexer_client,
            ckb_client,
            offchain: OffchainCellCollector::default(),
        }
    }

    /// Check if ckb-indexer synced with ckb node. This will check every 50ms for 10 times (500ms in total).
    pub fn check_ckb_chain(&mut self) -> Result<(), CellCollectorError> {
        let tip_header = self
            .ckb_client
            .get_tip_header()
            .map_err(|err| CellCollectorError::Internal(err.into()))?;
        let tip_hash = tip_header.hash;
        let tip_number = tip_header.inner.number;
        let mut retry = 10;
        while retry > 0 {
            match self
                .indexer_client
                .get_tip()
                .map_err(|err| CellCollectorError::Internal(err.into()))?
            {
                Some(Tip {
                    block_hash,
                    block_number,
                }) => {
                    if tip_number.value() > block_number.value() {
                        thread::sleep(Duration::from_millis(50));
                        retry -= 1;
                        continue;
                    } else if tip_hash == block_hash && tip_number == block_number {
                        return Ok(());
                    } else {
                        return Err(CellCollectorError::Other("ckb-indexer server inconsistent with currently connected ckb node or not synced!".to_owned().into()));
                    }
                }
                None => {
                    return Err(CellCollectorError::Other(
                        "ckb-indexer server not synced".to_owned().into(),
                    ));
                }
            }
        }
        Err(CellCollectorError::Other(
            "ckb-indexer server inconsistent with currently connected ckb node or not synced!"
                .to_owned()
                .into(),
        ))
    }
}

impl CellCollector for DefaultCellCollector {
    fn collect_live_cells(
        &mut self,
        query: &CellQueryOptions,
        apply_changes: bool,
    ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
        let max_mature_number = get_max_mature_number(&mut self.ckb_client)
            .map_err(|err| CellCollectorError::Internal(err.into()))?;

        self.offchain.max_mature_number = max_mature_number;
        let (mut cells, rest_cells, mut total_capacity) = self.offchain.collect(query);

        if total_capacity < query.min_total_capacity {
            self.check_ckb_chain()?;
            let order = match query.order {
                QueryOrder::Asc => Order::Asc,
                QueryOrder::Desc => Order::Desc,
            };
            let locked_cells = self.offchain.locked_cells.clone();
            let search_key = SearchKey::from(query.clone());
            const MAX_LIMIT: u32 = 4096;
            let mut limit: u32 = query.limit.unwrap_or(128);
            let mut last_cursor: Option<json_types::JsonBytes> = None;
            while total_capacity < query.min_total_capacity {
                let page = self
                    .indexer_client
                    .get_cells(search_key.clone(), order.clone(), limit.into(), last_cursor)
                    .map_err(|err| CellCollectorError::Internal(err.into()))?;
                if page.objects.is_empty() {
                    break;
                }
                for cell in page.objects {
                    let live_cell = LiveCell::from(cell);
                    if !query.match_cell(&live_cell, max_mature_number)
                        || locked_cells.contains(&(
                            live_cell.out_point.tx_hash().unpack(),
                            live_cell.out_point.index().unpack(),
                        ))
                    {
                        continue;
                    }
                    let capacity: u64 = live_cell.output.capacity().unpack();
                    total_capacity += capacity;
                    cells.push(live_cell);
                    if total_capacity >= query.min_total_capacity {
                        break;
                    }
                }
                last_cursor = Some(page.last_cursor);
                if limit < MAX_LIMIT {
                    limit *= 2;
                }
            }
        }
        if apply_changes {
            self.offchain.live_cells = rest_cells;
            for cell in &cells {
                self.lock_cell(cell.out_point.clone())?;
            }
        }
        Ok((cells, total_capacity))
    }

    fn lock_cell(&mut self, out_point: OutPoint) -> Result<(), CellCollectorError> {
        self.offchain.lock_cell(out_point)
    }
    fn apply_tx(&mut self, tx: Transaction) -> Result<(), CellCollectorError> {
        self.offchain.apply_tx(tx)
    }
    fn reset(&mut self) {
        self.offchain.reset();
    }
}

struct DefaultTxDepProviderInner {
    rpc_client: CkbRpcClient,
    tx_cache: LruCache<Byte32, TransactionView>,
    cell_cache: LruCache<OutPoint, (CellOutput, Bytes)>,
    header_cache: LruCache<Byte32, HeaderView>,
}

/// A transaction dependency provider use ckb rpc client as backend, and with LRU cache supported
pub struct DefaultTransactionDependencyProvider {
    // since we will mainly deal with LruCache, so use Mutex here
    inner: Arc<Mutex<DefaultTxDepProviderInner>>,
}

impl Clone for DefaultTransactionDependencyProvider {
    fn clone(&self) -> DefaultTransactionDependencyProvider {
        let inner = Arc::clone(&self.inner);
        DefaultTransactionDependencyProvider { inner }
    }
}

impl DefaultTransactionDependencyProvider {
    /// Arguments:
    ///   * `url` is the ckb http jsonrpc server url
    ///   * When `cache_capacity` is 0 for not using cache.
    pub fn new(url: &str, cache_capacity: usize) -> DefaultTransactionDependencyProvider {
        let rpc_client = CkbRpcClient::new(url);
        let inner = DefaultTxDepProviderInner {
            rpc_client,
            tx_cache: LruCache::new(cache_capacity),
            cell_cache: LruCache::new(cache_capacity),
            header_cache: LruCache::new(cache_capacity),
        };
        DefaultTransactionDependencyProvider {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    pub fn get_cell_with_data(
        &self,
        out_point: &OutPoint,
    ) -> Result<(CellOutput, Bytes), TransactionDependencyError> {
        let mut inner = self.inner.lock();
        if let Some(pair) = inner.cell_cache.get(out_point) {
            return Ok(pair.clone());
        }
        // TODO: handle proposed/pending transactions
        let cell_with_status = inner
            .rpc_client
            .get_live_cell(out_point.clone().into(), true)
            .map_err(|err| TransactionDependencyError::Other(err.into()))?;
        if cell_with_status.status != "live" {
            return Err(TransactionDependencyError::Other(
                format!("invalid cell status: {:?}", cell_with_status.status).into(),
            ));
        }
        let cell = cell_with_status.cell.unwrap();
        let output = CellOutput::from(cell.output);
        let output_data = cell.data.unwrap().content.into_bytes();
        inner
            .cell_cache
            .put(out_point.clone(), (output.clone(), output_data.clone()));
        Ok((output, output_data))
    }
}

impl TransactionDependencyProvider for DefaultTransactionDependencyProvider {
    fn get_transaction(
        &self,
        tx_hash: &Byte32,
    ) -> Result<TransactionView, TransactionDependencyError> {
        let mut inner = self.inner.lock();
        if let Some(tx) = inner.tx_cache.get(tx_hash) {
            return Ok(tx.clone());
        }
        // TODO: handle proposed/pending transactions
        let tx_with_status = inner
            .rpc_client
            .get_transaction(tx_hash.unpack())
            .map_err(|err| TransactionDependencyError::Other(err.into()))?
            .ok_or_else(|| TransactionDependencyError::NotFound("transaction".to_string()))?;
        if tx_with_status.tx_status.status != json_types::Status::Committed {
            return Err(TransactionDependencyError::Other(
                format!("invalid transaction status: {:?}", tx_with_status.tx_status).into(),
            ));
        }
        let tx = Transaction::from(tx_with_status.transaction.unwrap().inner).into_view();
        inner.tx_cache.put(tx_hash.clone(), tx.clone());
        Ok(tx)
    }
    fn get_cell(&self, out_point: &OutPoint) -> Result<CellOutput, TransactionDependencyError> {
        self.get_cell_with_data(out_point).map(|(output, _)| output)
    }
    fn get_cell_data(&self, out_point: &OutPoint) -> Result<Bytes, TransactionDependencyError> {
        self.get_cell_with_data(out_point)
            .map(|(_, output_data)| output_data)
    }
    fn get_header(&self, block_hash: &Byte32) -> Result<HeaderView, TransactionDependencyError> {
        let mut inner = self.inner.lock();
        if let Some(header) = inner.header_cache.get(block_hash) {
            return Ok(header.clone());
        }
        let header = inner
            .rpc_client
            .get_header(block_hash.unpack())
            .map_err(|err| TransactionDependencyError::Other(err.into()))?
            .map(HeaderView::from)
            .ok_or_else(|| TransactionDependencyError::NotFound("header".to_string()))?;
        inner.header_cache.put(block_hash.clone(), header.clone());
        Ok(header)
    }
}

/// A signer use secp256k1 raw key, the id is `blake160(pubkey)`.
#[derive(Default, Clone)]
pub struct SecpCkbRawKeySigner {
    keys: HashMap<H160, secp256k1::SecretKey>,
}

impl SecpCkbRawKeySigner {
    pub fn new(keys: HashMap<H160, secp256k1::SecretKey>) -> SecpCkbRawKeySigner {
        SecpCkbRawKeySigner { keys }
    }
    pub fn new_with_secret_keys(keys: Vec<secp256k1::SecretKey>) -> SecpCkbRawKeySigner {
        let mut signer = SecpCkbRawKeySigner::default();
        for key in keys {
            signer.add_secret_key(key);
        }
        signer
    }
    pub fn add_secret_key(&mut self, key: secp256k1::SecretKey) {
        let pubkey = secp256k1::PublicKey::from_secret_key(&SECP256K1, &key);
        let hash160 = H160::from_slice(&blake2b_256(&pubkey.serialize()[..])[0..20])
            .expect("Generate hash(H160) from pubkey failed");
        self.keys.insert(hash160, key);
    }
}

impl Signer for SecpCkbRawKeySigner {
    fn match_id(&self, id: &[u8]) -> bool {
        id.len() == 20 && self.keys.contains_key(&H160::from_slice(id).unwrap())
    }

    fn sign(
        &self,
        id: &[u8],
        message: &[u8],
        recoverable: bool,
        _tx: &TransactionView,
    ) -> Result<Bytes, SignerError> {
        if !self.match_id(id) {
            return Err(SignerError::IdNotFound);
        }
        if message.len() != 32 {
            return Err(SignerError::InvalidMessage(format!(
                "expected length: 32, got: {}",
                message.len()
            )));
        }
        let msg = secp256k1::Message::from_slice(message).expect("Convert to message failed");
        let key = self.keys.get(&H160::from_slice(id).unwrap()).unwrap();
        if recoverable {
            let sig = SECP256K1.sign_recoverable(&msg, key);
            Ok(Bytes::from(serialize_signature(&sig).to_vec()))
        } else {
            let sig = SECP256K1.sign(&msg, key);
            Ok(Bytes::from(sig.serialize_compact().to_vec()))
        }
    }
}
impl Drop for SecpCkbRawKeySigner {
    fn drop(&mut self) {
        for (_, mut secret_key) in self.keys.drain() {
            zeroize_privkey(&mut secret_key);
        }
    }
}
