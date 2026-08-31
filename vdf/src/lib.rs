pub mod discriminant;
pub mod error;
pub mod form;
pub mod gmp_form;
mod limbs;
pub mod proof;
pub mod validation;

pub use crate::discriminant::{create_discriminant, create_discriminant_bytes};
pub use crate::error::{Error, Result};
pub use crate::proof::{prove, verify_n_wesolowski, verify_vdf, verify_vdf_serial};
pub use crate::validation::{
    default_classgroup_element, validate_vdf_info, validate_vdf_info_result,
    validate_vdf_info_serial, validate_vdf_with_normalization,
    validate_vdf_with_normalization_result,
};

fn _version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
fn _pkg_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[must_use]
pub fn version() -> String {
    format!("{}: {}", _pkg_name(), _version())
}

#[test]
fn test_version() {
    println!("{}", version());
}

/// Test-surface exports for the BPSW differential gates: the native verdict and the
/// reference (rug/GMP) verdict, so integration tests can compare them at scale.
pub mod testing {
    use num_bigint::BigUint;

    pub fn is_probable_prime_native(n: &BigUint) -> bool {
        crate::discriminant::is_probable_prime_native(n)
    }

    pub fn is_probable_prime_reference(n: &BigUint) -> bool {
        let g = rug::Integer::from_digits(&n.to_bytes_be(), rug::integer::Order::MsfBe);
        g.is_probably_prime(24) != rug::integer::IsPrime::No
    }
}
