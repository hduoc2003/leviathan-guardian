use miden_protocol::Felt;

/// Maps an arbitrary `u64` onto a canonical field element by reducing modulo
/// the field order.
///
/// Byte-packed digest inputs are arbitrary `u64`s, so reducing here preserves
/// the original digest layout.
pub fn felt_from_u64_reduced(value: u64) -> Felt {
    Felt::new(value % Felt::ORDER)
}
