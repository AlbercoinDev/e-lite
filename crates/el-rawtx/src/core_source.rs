//! Local Bitcoin Core block source: zero external requests.

use crate::{BlockSource, RawtxError, Result};
use el_model::Hash32;
use el_rpc::CoreClient;

pub struct CoreBlockSource {
    client: CoreClient,
}

impl CoreBlockSource {
    pub fn new(client: CoreClient) -> Self {
        CoreBlockSource { client }
    }
}

impl BlockSource for CoreBlockSource {
    fn name(&self) -> &str {
        "core"
    }

    fn is_local(&self) -> bool {
        true
    }

    fn fetch_block(&self, hash: &Hash32) -> Result<Vec<u8>> {
        let hex = self
            .client
            .block_hex(&hash.to_hex())
            .map_err(|e| RawtxError::Unavailable(format!("core: {e}")))?;
        hex::decode(&hex).map_err(|e| RawtxError::Io(format!("core block hex: {e}")))
    }
}
