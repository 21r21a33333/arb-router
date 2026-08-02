#[derive(Clone, Debug, PartialEq)]
pub struct Bytes(pub Vec<u8>);

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub target: String,
    pub calldata: Bytes,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CallResult {
    pub success: bool,
    pub data: Bytes,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BlockId {
    Latest,
    Number(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct BatchOutput {
    pub block: u64,
    pub results: Vec<CallResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_result_reports_failure() {
        let r = CallResult {
            success: false,
            data: Bytes(vec![]),
        };
        assert!(!r.success);
    }
}
