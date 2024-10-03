use crate::poly::{Poly, Representation};
use crate::{BfvParameters, PolyType};
use itertools::Itertools;
use ndarray::{Array2, ArrayView1};
use num_bigint::BigInt;
use num_traits::ToPrimitive;
use num_traits::{FromPrimitive, Zero};
use traits::{Ntt, TryDecodingWithParameters, TryEncodingWithParameters};

#[derive(PartialEq, Clone)]
pub enum EncodingType {
    Simd,
    Poly,
}

#[derive(PartialEq, Clone)]
pub enum PolyCache {
    /// Supports scalar multiplications
    Mul(PolyType),
    /// Supports additions, subtractions
    /// Only supports PolyType::Q
    AddSub(Representation),
    /// Supports both
    All(PolyType, Representation),
    /// Used for encryption
    None,
}

#[derive(Clone)]
pub struct Encoding {
    pub encoding_type: EncodingType,
    pub poly_cache: PolyCache,
    pub level: usize,
}

impl Encoding {
    pub fn simd(level: usize, poly_cache: PolyCache) -> Encoding {
        Encoding {
            encoding_type: EncodingType::Simd,
            poly_cache,
            level,
        }
    }
}

impl Default for Encoding {
    fn default() -> Self {
        Encoding {
            encoding_type: EncodingType::Simd,
            poly_cache: PolyCache::None,
            level: 0,
        }
    }
}

#[derive(Clone)]
pub struct Plaintext {
    pub(crate) m: Vec<u64>,
    pub(crate) encoding: Option<Encoding>,
    pub(crate) mul_poly: Option<Poly>,
    pub(crate) add_sub_poly: Option<Poly>,
}

impl Plaintext {
    /// Encodes a given message `m` to plaintext using given `encoding`
    ///
    /// Panics if `m` values length is greater than polynomial degree
    pub fn encode(m: &[u64], params: &BfvParameters, encoding: Encoding) -> Plaintext {
        assert!(m.len() <= params.degree);

        let mut m1 = vec![0u64; params.degree];
        let m = m.to_vec();

        m.iter().enumerate().for_each(|(i, v)| {
            if encoding.encoding_type == EncodingType::Simd {
                m1[params.matrix_reps_index_map[i]] = *v;
            } else {
                m1[i] = *v;
            }
        });
        params.plaintext_modulus_op.reduce_vec(&mut m1);

        if encoding.encoding_type == EncodingType::Simd {
            params.plaintext_ntt_op.backward(&mut m1);
        }

        // convert m to polynomial with poly context at specific level
        let (mul_poly, add_sub_poly) = {
            match &encoding.poly_cache {
                PolyCache::Mul(poly_type) => {
                    let ctx = params.poly_ctx(poly_type, encoding.level);
                    let mut mul_poly = ctx.try_convert_from_u64(&m1, Representation::Coefficient);
                    ctx.change_representation(&mut mul_poly, Representation::Evaluation);
                    (Some(mul_poly), None)
                }
                PolyCache::AddSub(representation) => {
                    let poly = Plaintext::scale_m(&m1, params, &encoding, representation.clone());
                    (None, Some(poly))
                }
                PolyCache::All(poly_type, representation) => {
                    // mul
                    let ctx = params.poly_ctx(&poly_type, encoding.level);
                    let mut mul_poly = ctx.try_convert_from_u64(&m1, Representation::Coefficient);
                    ctx.change_representation(&mut mul_poly, Representation::Evaluation);

                    // add + sub
                    let add_sub_poly =
                        Plaintext::scale_m(&m1, params, &encoding, representation.clone());

                    (Some(mul_poly), Some(add_sub_poly))
                }
                PolyCache::None => (None, None),
            }
        };

        Plaintext {
            m: m1,
            encoding: Some(encoding),
            mul_poly: mul_poly,
            add_sub_poly: add_sub_poly,
        }
    }

    pub fn decode<T: Zero + Clone + FromPrimitive>(
        &self,
        encoding: Encoding,
        params: &BfvParameters,
    ) -> Vec<T> {
        assert!(self.encoding.is_none());

        let mut m1 = self.m.clone();
        if encoding.encoding_type == EncodingType::Simd {
            params.plaintext_ntt_op.forward(&mut m1);
        }

        let mut m = vec![T::zero(); params.degree];
        for i in 0..params.degree {
            if encoding.encoding_type == EncodingType::Simd {
                m[i] = T::from_u64(m1[params.matrix_reps_index_map[i]]).unwrap();
            } else {
                m[i] = T::from_u64(m1[i]).unwrap();
            }
        }

        m
    }

    /// Returns scaled polynomial `[round((Ql*[m]_t)/t)]_Ql`
    ///
    /// With remark 3.1 of https://eprint.iacr.org/2021/204.pdf one can calculate `round(Qm/t)` directly in RNS as [[Qlm]_t * [-t^{-1}]_Ql]_Ql
    ///
    /// Panics if encoding is not specified
    pub fn scale_m(
        m: &[u64],
        params: &BfvParameters,
        encoding: &Encoding,
        representation: Representation,
    ) -> Poly {
        let modt = &params.plaintext_modulus_op;

        let mut m = m.to_vec();
        modt.scalar_mul_mod_fast_vec(&mut m, params.ql_modt[encoding.level]);

        let ctx = params.poly_ctx(&PolyType::Q, encoding.level);

        let mut m_scaled_by_delta: Vec<u64> = Vec::new();
        for qi in ctx.moduli_ops() {
            let qi_modulus = BigInt::from(qi.modulus());
            let delta = BigInt::from(qi.inv(qi.neg_mod_fast(params.plaintext_modulus)));

            for x in m.iter() {
                // Scale by delta, reduce by modulus, and ensure result is non-negative
                let mut reduced =
                    BigInt::from(params.plaintext_modulus_op.center(*x)) * &delta % &qi_modulus;
                if reduced < BigInt::from(0) {
                    reduced += &qi_modulus;
                }

                // Convert to u64, panicking if the value is too large
                m_scaled_by_delta.push(
                    reduced
                        .to_u64()
                        .unwrap_or_else(|| panic!("Value {:?} too large for u64", reduced)),
                );
            }
        }
        let m_final =
        Array2::from_shape_vec((ctx.moduli_count, ctx.degree), m_scaled_by_delta)
            .unwrap();

        // [[Qr*m]_t * (-t)^{-1}]_Qr
        let mut m_poly = Poly::new(m_final, Representation::Coefficient);


        // An alternate method to this will be to store [-t_inv]_Q
        // and perform scalar multiplication [-t_inv]_Q with `m_poly`
        // in coefficient form.
        // We prefer this because `m_poly` needs to change representation
        // to `Evaluation` anyways.
        ctx.change_representation(&mut m_poly, Representation::Evaluation);
        // ctx.mul_assign(&mut m_poly, &params.neg_t_inv_modql[encoding.level]);

        if representation != Representation::Evaluation {
            ctx.change_representation(&mut m_poly, representation);
        }

        m_poly
    }

    pub fn mul_poly_type(&self) -> PolyType {
        match &self.encoding.as_ref().unwrap().poly_cache {
            PolyCache::Mul(poly_type) => poly_type.clone(),
            _ => {
                panic!("PolyCache not Mul")
            }
        }
    }

    pub fn level(&self) -> usize {
        self.encoding.as_ref().unwrap().level
    }

    pub fn supports_mul_poly(&self) -> bool {
        self.mul_poly.is_some()
    }

    pub fn add_sub_poly_ref(&self) -> &Poly {
        self.add_sub_poly.as_ref().expect("Missing add_sub poly")
    }

    pub fn mul_poly_ref(&self) -> &Poly {
        self.mul_poly.as_ref().expect("Missing mul poly")
    }

    pub fn move_mul_poly(self) -> Poly {
        self.mul_poly.expect("Missing mul poly")
    }

    pub fn move_add_sub_poly(self) -> Poly {
        self.add_sub_poly.expect("Missing add_sub poly")
    }

    pub fn value(&self) -> &[u64] {
        &self.m
    }        
}

impl TryEncodingWithParameters<&[u32]> for Plaintext {
    type Encoding = Encoding;
    type Parameters = BfvParameters;

    fn try_encoding_with_parameters(
        value: &[u32],
        parameters: &Self::Parameters,
        encoding: Self::Encoding,
    ) -> Self {
        let value_u64 = value.iter().map(|v| *v as u64).collect_vec();
        Self::encode(&value_u64, parameters, encoding)
    }
}

impl<'a> TryEncodingWithParameters<ArrayView1<'a, u32>> for Plaintext {
    type Encoding = Encoding;
    type Parameters = BfvParameters;

    fn try_encoding_with_parameters(
        value: ArrayView1<'a, u32>,
        parameters: &Self::Parameters,
        encoding: Self::Encoding,
    ) -> Self {
        let value_u64 = value.iter().map(|v| *v as u64).collect_vec();
        Self::encode(&value_u64, parameters, encoding)
    }
}

impl<'a> TryDecodingWithParameters<&'a Plaintext> for Vec<u32> {
    type Encoding = Encoding;
    type Parameters = &'a BfvParameters;

    fn try_decoding_with_parameters(
        value: &'a Plaintext,
        parameters: Self::Parameters,
        encoding: Self::Encoding,
    ) -> Vec<u32> {
        value.decode(encoding, parameters)
    }
}

#[cfg(test)]
mod tests {}
