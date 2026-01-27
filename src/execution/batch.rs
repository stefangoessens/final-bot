pub const MAX_BATCH_ORDERS: usize = 15;

pub fn chunk<T>(items: Vec<T>, max: usize) -> Vec<Vec<T>> {
    if items.is_empty() || max == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut current = Vec::with_capacity(max.min(items.len()));
    for item in items {
        current.push(item);
        if current.len() >= max {
            out.push(current);
            current = Vec::with_capacity(max);
        }
    }

    if !current.is_empty() {
        out.push(current);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_splits_into_batches() {
        let items: Vec<u64> = (0..17).collect();
        let batches = chunk(items, 15);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 15);
        assert_eq!(batches[1].len(), 2);
    }
}
