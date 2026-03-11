use alloy_primitives::B256;
use eyre::{Result, ensure};

#[cfg(test)]
mod tests {
    use super::find_first_mismatch;
    use eyre::Result;

    #[test]
    fn returns_first_divergent_block_for_monotonic_range() -> Result<()> {
        let mismatch_from = 103_u64;

        let got = find_first_mismatch(100, 105, |block| Ok(block < mismatch_from))?;

        assert_eq!(got, Some(mismatch_from));
        Ok(())
    }

    #[test]
    fn returns_none_when_everything_matches() -> Result<()> {
        let got = find_first_mismatch(100, 105, |_block| Ok(true))?;

        assert_eq!(got, None);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRootComparison {
    pub block_number: u64,
    pub reth_root: B256,
    pub geth_disk_root: B256,
}

impl BlockRootComparison {
    pub fn is_match(&self) -> bool {
        self.reth_root == self.geth_disk_root
    }
}

pub fn find_first_mismatch<F>(from: u64, to: u64, mut matches: F) -> Result<Option<u64>>
where
    F: FnMut(u64) -> Result<bool>,
{
    ensure!(
        from <= to,
        "invalid range: from-block {from} is greater than to-block {to}"
    );

    if !matches(from)? {
        return Ok(Some(from));
    }

    if matches(to)? {
        return Ok(None);
    }

    let mut low = from.saturating_add(1);
    let mut high = to;
    let mut first = to;

    while low <= high {
        let mid = low + (high - low) / 2;
        if matches(mid)? {
            low = mid.saturating_add(1);
        } else {
            first = mid;
            if mid == 0 {
                break;
            }
            high = mid - 1;
        }
    }

    Ok(Some(first))
}
