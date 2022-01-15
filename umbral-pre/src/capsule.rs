use alloc::vec::Vec;
use core::fmt;

use generic_array::sequence::Concat;
use generic_array::GenericArray;
use rand_core::{CryptoRng, RngCore};
use typenum::op;

#[cfg(feature = "serde-support")]
use crate::serde::{serde_deserialize, serde_serialize, Representation};

use crate::capsule_frag::CapsuleFrag;
use crate::curve::{CurvePoint, CurveScalar, NonZeroCurveScalar};
use crate::hashing_ds::{hash_capsule_points, hash_to_polynomial_arg, hash_to_shared_secret};
use crate::keys::{PublicKey, SecretKey};
use crate::params::Parameters;
use crate::secret_box::SecretBox;
use crate::traits::{
    fmt_public, ConstructionError, DeserializableFromArray, HasTypeName, RepresentableAsArray,
    SerializableToArray,
};

#[cfg(feature = "serde-support")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Errors that can happen when opening a `Capsule` using reencrypted `CapsuleFrag` objects.
#[derive(Debug, PartialEq)]
pub enum OpenReencryptedError {
    /// An empty capsule fragment list is given.
    NoCapsuleFrags,
    /// Capsule fragments are mismatched (originated from [`KeyFrag`](crate::KeyFrag) objects
    /// generated by different [`generate_kfrags`](crate::generate_kfrags) calls).
    MismatchedCapsuleFrags,
    /// Some of the given capsule fragments are repeated.
    RepeatingCapsuleFrags,
    /// Internal validation of the result has failed.
    /// Can be caused by an incorrect (possibly modified) capsule
    /// or some of the capsule fragments.
    ValidationFailed,
}

impl fmt::Display for OpenReencryptedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCapsuleFrags => write!(f, "Empty CapsuleFrag sequence"),
            Self::MismatchedCapsuleFrags => write!(f, "CapsuleFrags are not pairwise consistent"),
            Self::RepeatingCapsuleFrags => write!(f, "Some of the CapsuleFrags are repeated"),
            Self::ValidationFailed => write!(f, "Internal validation failed"),
        }
    }
}

/// Encapsulated symmetric key used to encrypt the plaintext.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Capsule {
    pub(crate) params: Parameters,
    pub(crate) point_e: CurvePoint,
    pub(crate) point_v: CurvePoint,
    pub(crate) signature: CurveScalar,
}

type PointSize = <CurvePoint as RepresentableAsArray>::Size;
type ScalarSize = <CurveScalar as RepresentableAsArray>::Size;

impl RepresentableAsArray for Capsule {
    type Size = op!(PointSize + PointSize + ScalarSize);
}

impl SerializableToArray for Capsule {
    fn to_array(&self) -> GenericArray<u8, Self::Size> {
        self.point_e
            .to_array()
            .concat(self.point_v.to_array())
            .concat(self.signature.to_array())
    }
}

impl DeserializableFromArray for Capsule {
    fn from_array(arr: &GenericArray<u8, Self::Size>) -> Result<Self, ConstructionError> {
        let (point_e, rest) = CurvePoint::take(*arr)?;
        let (point_v, rest) = CurvePoint::take(rest)?;
        let signature = CurveScalar::take_last(rest)?;
        Self::new_verified(point_e, point_v, signature)
            .ok_or_else(|| ConstructionError::new("Capsule", "Self-verification failed"))
    }
}

#[cfg(feature = "serde-support")]
#[cfg_attr(docsrs, doc(cfg(feature = "serde-support")))]
impl Serialize for Capsule {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serde_serialize(self, serializer, Representation::Base64)
    }
}

#[cfg(feature = "serde-support")]
#[cfg_attr(docsrs, doc(cfg(feature = "serde-support")))]
impl<'de> Deserialize<'de> for Capsule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        serde_deserialize(deserializer, Representation::Base64)
    }
}

impl HasTypeName for Capsule {
    fn type_name() -> &'static str {
        "Capsule"
    }
}

impl fmt::Display for Capsule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_public::<Self>(self, f)
    }
}

pub(crate) type KeySeed = GenericArray<u8, <CurvePoint as RepresentableAsArray>::Size>;

impl Capsule {
    fn new(point_e: CurvePoint, point_v: CurvePoint, signature: CurveScalar) -> Self {
        let params = Parameters::new();
        Self {
            params,
            point_e,
            point_v,
            signature,
        }
    }

    pub(crate) fn new_verified(
        point_e: CurvePoint,
        point_v: CurvePoint,
        signature: CurveScalar,
    ) -> Option<Self> {
        let capsule = Self::new(point_e, point_v, signature);
        match capsule.verify() {
            false => None,
            true => Some(capsule),
        }
    }

    /// Verifies the integrity of the capsule.
    fn verify(&self) -> bool {
        let g = CurvePoint::generator();
        let h = hash_capsule_points(&self.point_e, &self.point_v);
        &g * &self.signature == &self.point_v + &(&self.point_e * &h)
    }

    /// Generates a symmetric key and its associated KEM ciphertext, using the given RNG.
    pub(crate) fn from_public_key(
        rng: &mut (impl CryptoRng + RngCore),
        delegating_pk: &PublicKey,
    ) -> (Capsule, SecretBox<KeySeed>) {
        let g = CurvePoint::generator();

        let priv_r = SecretBox::new(NonZeroCurveScalar::random(rng));
        let pub_r = &g * priv_r.as_secret();

        let priv_u = SecretBox::new(NonZeroCurveScalar::random(rng));
        let pub_u = &g * priv_u.as_secret();

        let h = hash_capsule_points(&pub_r, &pub_u);

        let s = priv_u.as_secret() + &(priv_r.as_secret() * &h);

        let shared_key =
            SecretBox::new(&delegating_pk.to_point() * &(priv_r.as_secret() + priv_u.as_secret()));

        let capsule = Self::new(pub_r, pub_u, s);

        (capsule, SecretBox::new(shared_key.as_secret().to_array()))
    }

    /// Derive the same symmetric key
    pub(crate) fn open_original(&self, delegating_sk: &SecretKey) -> SecretBox<KeySeed> {
        let shared_key = SecretBox::new(
            &(&self.point_e + &self.point_v) * delegating_sk.to_secret_scalar().as_secret(),
        );
        SecretBox::new(shared_key.as_secret().to_array())
    }

    #[allow(clippy::many_single_char_names)]
    pub(crate) fn open_reencrypted(
        &self,
        receiving_sk: &SecretKey,
        delegating_pk: &PublicKey,
        cfrags: &[CapsuleFrag],
    ) -> Result<SecretBox<KeySeed>, OpenReencryptedError> {
        if cfrags.is_empty() {
            return Err(OpenReencryptedError::NoCapsuleFrags);
        }

        let precursor = cfrags[0].precursor;

        if !cfrags.iter().all(|cfrag| cfrag.precursor == precursor) {
            return Err(OpenReencryptedError::MismatchedCapsuleFrags);
        }

        let pub_key = receiving_sk.public_key().to_point();
        let dh_point = &precursor * receiving_sk.to_secret_scalar().as_secret();

        // Combination of CFrags via Shamir's Secret Sharing reconstruction
        let mut lc = Vec::<NonZeroCurveScalar>::with_capacity(cfrags.len());
        for cfrag in cfrags {
            let coeff = hash_to_polynomial_arg(&precursor, &pub_key, &dh_point, &cfrag.kfrag_id);
            lc.push(coeff);
        }

        let mut e_prime = CurvePoint::identity();
        let mut v_prime = CurvePoint::identity();
        for (i, cfrag) in cfrags.iter().enumerate() {
            // There is a minuscule probability that coefficients for two different frags are equal,
            // in which case we'd rather fail gracefully.
            let lambda_i =
                lambda_coeff(&lc, i).ok_or(OpenReencryptedError::RepeatingCapsuleFrags)?;
            e_prime = &e_prime + &(&cfrag.point_e1 * &lambda_i);
            v_prime = &v_prime + &(&cfrag.point_v1 * &lambda_i);
        }

        // Secret value 'd' allows to make Umbral non-interactive
        let d = hash_to_shared_secret(&precursor, &pub_key, &dh_point);

        let s = self.signature;
        let h = hash_capsule_points(&self.point_e, &self.point_v);

        let orig_pub_key = delegating_pk.to_point();

        let inv_d = d.invert();

        if &orig_pub_key * &(&s * &inv_d) != &(&e_prime * &h) + &v_prime {
            return Err(OpenReencryptedError::ValidationFailed);
        }

        let shared_key = SecretBox::new(&(&e_prime + &v_prime) * &d);
        Ok(SecretBox::new(shared_key.as_secret().to_array()))
    }
}

fn lambda_coeff(xs: &[NonZeroCurveScalar], i: usize) -> Option<CurveScalar> {
    let mut res = CurveScalar::one();
    for j in 0..xs.len() {
        if j != i {
            let inv_diff_opt: Option<CurveScalar> = (&xs[j] - &xs[i]).invert().into();
            let inv_diff = inv_diff_opt?;
            res = &(&res * &xs[j]) * &inv_diff;
        }
    }
    Some(res)
}

#[cfg(test)]
mod tests {

    use alloc::vec::Vec;

    use rand_core::OsRng;

    use super::{Capsule, OpenReencryptedError};

    use crate::{
        encrypt, generate_kfrags, reencrypt, DeserializableFromArray, SecretKey,
        SerializableToArray, Signer,
    };

    #[cfg(feature = "serde-support")]
    use crate::serde::tests::{check_deserialization, check_serialization};

    #[cfg(feature = "serde-support")]
    use crate::serde::Representation;

    #[test]
    fn test_serialize() {
        let delegating_sk = SecretKey::random();
        let delegating_pk = delegating_sk.public_key();

        let plaintext = b"peace at dawn";
        let (capsule, _ciphertext) = encrypt(&delegating_pk, plaintext).unwrap();

        let capsule_arr = capsule.to_array();
        let capsule_back = Capsule::from_array(&capsule_arr).unwrap();
        assert_eq!(capsule, capsule_back);
    }

    #[test]
    fn test_open_reencrypted() {
        let delegating_sk = SecretKey::random();
        let delegating_pk = delegating_sk.public_key();

        let signer = Signer::new(SecretKey::random());

        let receiving_sk = SecretKey::random();
        let receiving_pk = receiving_sk.public_key();

        let (capsule, key_seed) = Capsule::from_public_key(&mut OsRng, &delegating_pk);

        let kfrags = generate_kfrags(&delegating_sk, &receiving_pk, &signer, 2, 3, true, true);

        let vcfrags: Vec<_> = kfrags
            .iter()
            .map(|kfrag| reencrypt(&capsule, &kfrag))
            .collect();

        let cfrags: Vec<_> = vcfrags
            .iter()
            .map(|vcfrag| vcfrag.to_unverified())
            .collect();

        let key_seed_reenc = capsule
            .open_reencrypted(&receiving_sk, &delegating_pk, &cfrags)
            .unwrap();
        assert_eq!(key_seed.as_secret(), key_seed_reenc.as_secret());

        // Empty cfrag vector
        let result = capsule.open_reencrypted(&receiving_sk, &delegating_pk, &[]);
        assert_eq!(
            result.map(|x| x.as_secret().clone()),
            Err(OpenReencryptedError::NoCapsuleFrags)
        );

        // Mismatched cfrags - each `generate_kfrags()` uses new randoms.
        let kfrags2 = generate_kfrags(&delegating_sk, &receiving_pk, &signer, 2, 3, true, true);

        let vcfrags2: Vec<_> = kfrags2
            .iter()
            .map(|kfrag| reencrypt(&capsule, &kfrag))
            .collect();

        let mismatched_cfrags: Vec<_> = vcfrags[0..1]
            .iter()
            .cloned()
            .chain(vcfrags2[1..2].iter().cloned())
            .map(|vcfrag| vcfrag.to_unverified())
            .collect();

        let result = capsule.open_reencrypted(&receiving_sk, &delegating_pk, &mismatched_cfrags);
        assert_eq!(
            result.map(|x| x.as_secret().clone()),
            Err(OpenReencryptedError::MismatchedCapsuleFrags)
        );

        // Mismatched capsule
        let (capsule2, _key_seed) = Capsule::from_public_key(&mut OsRng, &delegating_pk);
        let result = capsule2.open_reencrypted(&receiving_sk, &delegating_pk, &cfrags);
        assert_eq!(
            result.map(|x| x.as_secret().clone()),
            Err(OpenReencryptedError::ValidationFailed)
        );
    }

    #[cfg(feature = "serde-support")]
    #[test]
    fn test_serde_serialization() {
        let delegating_sk = SecretKey::random();
        let delegating_pk = delegating_sk.public_key();
        let (capsule, _key_seed) = Capsule::from_public_key(&mut OsRng, &delegating_pk);

        check_serialization(&capsule, Representation::Base64);
        check_deserialization(&capsule);
    }
}
