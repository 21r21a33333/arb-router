/// A block to read at: the chain head, or a pinned height.
#[derive(Clone, Debug, PartialEq)]
pub enum BlockId {
    Latest,
    Number(u64),
}
