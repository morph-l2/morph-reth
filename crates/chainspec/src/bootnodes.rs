//! Morph boot nodes.
//!
//! Source: <https://github.com/morph-l2/go-ethereum/blob/main/params/bootnodes.go>

use reth_network_peers::{NodeRecord, parse_nodes};

/// Morph Mainnet boot nodes.
pub(crate) static MORPH_MAINNET_BOOTNODES: [&str; 7] = [
    "enode://53496aab21ab73f261551d823340b417ecd2cf69652ceba670162b990429e75c4dd6eb622dd96978d65f70c9db9964b3f477886c0b62297a51266168d367bab2@52.193.82.123:30303",
    "enode://58773a8b58e70cda8d39ed7500b56f176b22c5d4faab88348a0835f333f25ae3b3a45bf7f3e9fd08245d1ff57d38d01540571c0221be2404ce93568f8472c3a0@52.197.80.227:30303",
    "enode://86faa94f8221ae100c6f3aebc2f79941431ce2178f1f4374a73511230846bc26df4c336139e766668f5ba8daea56fdce74f61ecc31a2cba3788e70a2d15f5258@18.177.252.130:30303",
    "enode://3e3b6350e0392ace589163e3c26dc27e142acc0e5975a1df57b0da21f1aa27c756dcdb82188a754597f6815e56fce706b033fdbc04e0fefa12b53bcee66b7a0f@18.118.70.50:30303",
    "enode://aecc4b91f0a7b1f46a26021f33b0bf0bcadd461312a3140f9dc46dcac82e9943e7e485206862dce2b1d1f210742268b12b048c113ee8be1cf70b29012a22b920@3.134.21.91:30303",
    "enode://352f9f59fd1c65442917b81e5b5a78d9590d14921aafc59830d231d34df066da1aff0e986ed272f1cbc76527ac539fbbda8c461e2545389b6cf97f9b26bc3199@3.127.133.228:30303",
    "enode://abc60fccd847e9d9d9e5865aa69cef051239f01fa1108ba6a68082d5942f52e560452aeeffbe1aeca88a33afc5a0d97da9ba2111eb825c92cdd5d38727d36e1e@18.199.61.121:30303",
];

/// Morph Hoodi Testnet boot nodes.
pub(crate) static MORPH_HOODI_BOOTNODES: [&str; 3] = [
    "enode://8efa3da017d3eeb9db761e17c10121ee3ee6d258045ac4fba42549552376547f818a31e933b9dd904528aaadc9b6b457e9d1970e3cbbad42d7a0171686cbd994@13.159.40.158:30303",
    "enode://884f27d218751e1f64eec36f7a5bad2bd03006d0a73b9557e36bb90847db5cb8cc2f86cb88121cecaf8599779ec088a9670fec8ee8611885591a97b52064f171@18.177.181.171:30303",
    "enode://cc0a69714111d69bb6f664c06d4d8a560019259ba9828ffd5714d465bc334652354b1660b6c058c50821f790d2f76a005bf0a3c74b55cce44b7e10180130fac3@13.230.212.70:30303",
];

pub(crate) fn morph_mainnet_nodes() -> Vec<NodeRecord> {
    parse_nodes(MORPH_MAINNET_BOOTNODES)
}

pub(crate) fn morph_hoodi_nodes() -> Vec<NodeRecord> {
    parse_nodes(MORPH_HOODI_BOOTNODES)
}