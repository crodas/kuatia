//! Static asset registry for the demo. Kuatia tracks assets only by opaque
//! [`AssetId`]; symbols and decimal precision live in the application, so the
//! dashboard defines them here and exposes them over the API for formatting.

use kuatia_core::AssetId;
use serde::Serialize;

pub const USD: AssetId = AssetId::new(1);
pub const EUR: AssetId = AssetId::new(2);
pub const BTC: AssetId = AssetId::new(3);

/// Presentation metadata for one asset.
#[derive(Debug, Clone, Serialize)]
pub struct AssetMeta {
    pub id: AssetId,
    pub code: &'static str,
    pub symbol: &'static str,
    pub decimals: u8,
}

/// All assets known to the demo, in display order.
pub fn registry() -> Vec<AssetMeta> {
    vec![
        AssetMeta {
            id: USD,
            code: "USD",
            symbol: "$",
            decimals: 2,
        },
        AssetMeta {
            id: EUR,
            code: "EUR",
            symbol: "\u{20ac}",
            decimals: 2,
        },
        AssetMeta {
            id: BTC,
            code: "BTC",
            symbol: "\u{20bf}",
            decimals: 8,
        },
    ]
}
