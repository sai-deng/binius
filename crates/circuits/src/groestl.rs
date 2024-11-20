// Copyright 2024 Irreducible Inc.

use crate::{
	builder::ConstraintSystemBuilder, helpers::make_underliers, transparent,
	unconstrained::unconstrained,
};
use anyhow::Result;
use binius_core::oracle::OracleId;
use binius_field::{
	as_packed_field::{PackScalar, PackedType},
	packed::{get_packed_slice, set_packed_slice},
	underlier::WithUnderlier,
	AESTowerField8b, BinaryField1b, BinaryField8b, ExtensionField, Field, PackedField, TowerField,
};
use binius_math::CompositionPolyOS;
use bytemuck::{must_cast_slice, must_cast_slice_mut, Pod};
use rayon::prelude::*;
use std::{array, fmt::Debug, iter};

pub fn groestl_p_permutation<U, F, FBase>(
	builder: &mut ConstraintSystemBuilder<U, F, FBase>,
	log_size: usize,
) -> Result<[OracleId; STATE_SIZE]>
where
	U: PackScalar<F>
		+ PackScalar<FBase>
		+ PackScalar<BinaryField1b>
		+ PackScalar<AESTowerField8b>
		+ Pod,
	F: TowerField + ExtensionField<AESTowerField8b> + ExtensionField<FBase>,
	FBase: TowerField + ExtensionField<AESTowerField8b>,
	PackedType<U, F>: Pod,
{
	let p_in = array::try_from_fn(|i| {
		unconstrained::<U, F, FBase, AESTowerField8b>(builder, format!("p_in[{i}]"), log_size)
	})?;
	let multiples_16: [_; 8] = array::from_fn(|i| {
		transparent::constant(
			builder,
			format!("multiples_16[{i}]"),
			log_size,
			AESTowerField8b::new(i as u8 * 0x10),
		)
		.unwrap()
	});

	let round_consts = permutation_round_consts(builder, log_size, 0, multiples_16, p_in)?;
	let mut output =
		groestl_p_permutation_round(builder, "round[0]", log_size, round_consts, p_in)?;
	for round_index in 1..N_ROUNDS {
		let round_consts =
			permutation_round_consts(builder, log_size, round_index, multiples_16, output)?;
		output = groestl_p_permutation_round(
			builder,
			format!("rounds[{round_index}]"),
			log_size,
			round_consts,
			output,
		)?;
	}
	let p_out = output;

	#[cfg(debug_assertions)]
	if let Some(witness) = builder.witness() {
		use binius_field::PackedAESBinaryField64x8b;
		use binius_hash::Groestl256Core;

		let input_polys = p_in.try_map(|id| witness.get::<AESTowerField8b>(id))?;
		let inputs = input_polys
			.iter()
			.map(|p| WithUnderlier::to_underliers_ref(p.evals()))
			.map(must_cast_slice::<_, AESTowerField8b>)
			.collect::<Vec<_>>();

		let output_polys = p_out.try_map(|id| witness.get::<AESTowerField8b>(id))?;
		let outputs = output_polys
			.iter()
			.map(|p| WithUnderlier::to_underliers_ref(p.evals()))
			.map(must_cast_slice::<_, AESTowerField8b>)
			.collect::<Vec<_>>();

		for z in 0..1 << log_size {
			assert_eq!(
				Groestl256Core.permutation_p(PackedAESBinaryField64x8b::from_fn(|i| inputs[i][z])),
				PackedAESBinaryField64x8b::from_fn(|i| outputs[i][z])
			);
		}
	}

	Ok(p_out)
}

#[allow(clippy::needless_range_loop)]
fn groestl_p_permutation_round<U, F, FBase>(
	builder: &mut ConstraintSystemBuilder<U, F, FBase>,
	name: impl ToString,
	log_size: usize,
	round_consts: [OracleId; 8],
	input: [OracleId; STATE_SIZE],
) -> Result<[OracleId; STATE_SIZE]>
where
	U: PackScalar<F>
		+ PackScalar<FBase>
		+ PackScalar<BinaryField1b>
		+ PackScalar<AESTowerField8b>
		+ Pod,
	F: TowerField + ExtensionField<AESTowerField8b> + ExtensionField<FBase>,
	FBase: TowerField + ExtensionField<AESTowerField8b>,
{
	builder.push_namespace(name);

	let p_sub_bytes_out: [OracleId; STATE_SIZE] = array::try_from_fn(|i| {
		groestl_p_permutation_sbox(
			builder,
			format!("s_box[{i}]"),
			log_size,
			if i % 8 == 0 {
				round_consts[i / 8]
			} else {
				input[i]
			},
		)
	})?;

	// Shift and mix bytes using committed columns
	let output = builder.add_committed_multiple("output", log_size, BinaryField8b::TOWER_LEVEL);

	if let Some(witness) = builder.witness() {
		let mut output_witness = output.map(|_| make_underliers::<U, AESTowerField8b>(log_size));
		{
			let p_sub_bytes_out_poly =
				p_sub_bytes_out.try_map(|id| witness.get::<AESTowerField8b>(id))?;
			let p_sub_bytes_out = p_sub_bytes_out_poly
				.iter()
				.map(|p| WithUnderlier::to_underliers_ref(p.evals()))
				.collect::<Vec<_>>();

			let output = output_witness
				.each_mut()
				.map(|col| must_cast_slice_mut::<_, AESTowerField8b>(col));

			let two = AESTowerField8b::new(2);
			for z in 0..1 << log_size {
				for j in 0..8 {
					let a_j: [_; 8] = array::from_fn(|i| {
						let shift_p = ((i + j) % 8) * 8 + i; // ShiftBytes & MixBytes
						let x = p_sub_bytes_out[shift_p];
						let x_as_packed = PackedType::<U, AESTowerField8b>::from_underliers_ref(x);
						get_packed_slice(x_as_packed, z)
					});
					for i in 0..8 {
						let ij = j * 8 + i;
						let a_i: [AESTowerField8b; 8] = array::from_fn(|k| a_j[(i + k) % 8]);
						// Here we are using an optimized matrix multiplication, as documented in
						// section 4.4.2 of https://www.groestl.info/groestl-implementation-guide.pdf
						let b_ij = two
							* (two * (a_i[3] + a_i[4] + a_i[6] + a_i[7])
								+ a_i[0] + a_i[1] + a_i[2]
								+ a_i[5] + a_i[7]) + a_i[2]
							+ a_i[4] + a_i[5] + a_i[6]
							+ a_i[7];

						output[ij][z] = b_ij;
					}
				}
			}
		}
		witness.set_owned::<AESTowerField8b, _>(std::iter::zip(output, output_witness))?;
	}

	for ij in 0..STATE_SIZE {
		let i = ij / 8;
		let j = ij % 8;

		let mut mix_shift_oracles = [OracleId::default(); 9];
		mix_shift_oracles[0] = output[ij];
		for k in 0..8 {
			let j_prime = (j + k) % 8;
			let i_prime = (i + j_prime) % 8;
			mix_shift_oracles[k + 1] = p_sub_bytes_out[i_prime * 8 + j_prime];
		}
		// This is not required if the columns are virtual
		builder.assert_zero(mix_shift_oracles, MixColumn::<AESTowerField8b>::default());
	}

	builder.pop_namespace();
	Ok(output)
}

fn groestl_p_permutation_sbox<U, F, FBase>(
	builder: &mut ConstraintSystemBuilder<U, F, FBase>,
	name: impl ToString,
	log_size: usize,
	input: OracleId,
) -> Result<OracleId, anyhow::Error>
where
	U: PackScalar<F>
		+ PackScalar<FBase>
		+ PackScalar<BinaryField1b>
		+ PackScalar<AESTowerField8b>
		+ Pod,
	F: TowerField + ExtensionField<AESTowerField8b> + ExtensionField<FBase>,
	FBase: TowerField + ExtensionField<AESTowerField8b>,
{
	builder.push_namespace(name);
	let inv_bits: [OracleId; 8] =
		builder.add_committed_multiple("inv_bits", log_size, BinaryField1b::TOWER_LEVEL);

	let inv = builder.add_linear_combination(
		"inv",
		log_size,
		(0..8).map(|b| {
			let basis = <AESTowerField8b as ExtensionField<BinaryField1b>>::basis(b)
				.expect("index is less than extension degree");
			(inv_bits[b], basis.into())
		}),
	)?;

	let output = builder.add_linear_combination_with_offset(
		"output",
		log_size,
		SBOX_VEC.into(),
		(0..8).map(|b| (inv_bits[b], SBOX_MATRIX[b].into())),
	)?;

	if let Some(witness) = builder.witness() {
		let mut inv_bits_witness: [_; 8] =
			inv_bits.map(|_| make_underliers::<U, BinaryField1b>(log_size));
		let mut inv_witness = make_underliers::<U, AESTowerField8b>(log_size);
		let mut output_witness = make_underliers::<U, AESTowerField8b>(log_size);
		{
			let input_poly = witness.get::<AESTowerField8b>(input)?;
			let input = must_cast_slice::<_, AESTowerField8b>(WithUnderlier::to_underliers_ref(
				input_poly.evals(),
			));
			let inv_bits = inv_bits_witness
				.each_mut()
				.map(|bit| PackedType::<U, BinaryField1b>::from_underliers_ref_mut(bit));
			let inv = must_cast_slice_mut::<_, AESTowerField8b>(&mut inv_witness);
			let output = must_cast_slice_mut::<_, AESTowerField8b>(&mut output_witness);

			for z in 0..(1 << log_size) {
				inv[z] = input[z].invert_or_zero();
				output[z] = s_box(input[z]);
				let inv_bits_bases = ExtensionField::<BinaryField1b>::iter_bases(&inv[z]);
				for (b, bit) in inv_bits_bases.enumerate() {
					set_packed_slice(inv_bits[b], z, bit);
				}
			}
		}
		witness.set_owned::<BinaryField1b, _>(std::iter::zip(inv_bits, inv_bits_witness))?;
		witness.set_owned::<AESTowerField8b, _>([(inv, inv_witness), (output, output_witness)])?;
	}

	builder.assert_zero([input, inv], SBoxConstraint);
	builder.pop_namespace();
	Ok(output)
}

// TODO: Get rid of round constants and bake them into the constraints
fn permutation_round_consts<U, F, FBase>(
	builder: &mut ConstraintSystemBuilder<U, F, FBase>,
	log_size: usize,
	round_index: usize,
	multiples_16: [OracleId; 8],
	input: [OracleId; STATE_SIZE],
) -> Result<[OracleId; 8], anyhow::Error>
where
	U: PackScalar<F>
		+ PackScalar<FBase>
		+ PackScalar<BinaryField1b>
		+ PackScalar<AESTowerField8b>
		+ Pod,
	F: TowerField + ExtensionField<AESTowerField8b> + ExtensionField<FBase>,
	FBase: TowerField + ExtensionField<AESTowerField8b>,
{
	let round = transparent::constant(
		builder,
		format!("round_index[{round_index}]"),
		log_size,
		AESTowerField8b::new(round_index as u8),
	)?;

	let round_consts: [OracleId; 8] = array::try_from_fn(|i| {
		builder.add_linear_combination(
			format!("round_consts[{i}]"),
			log_size,
			[
				(input[8 * i], F::ONE),
				(round, F::ONE),
				(multiples_16[i], F::ONE),
			],
		)
	})?;
	if let Some(witness) = builder.witness() {
		let mut round_consts_witness: [_; 8] =
			round_consts.map(|_| make_underliers::<U, AESTowerField8b>(log_size));
		{
			let input = input.try_map(|id| witness.get::<AESTowerField8b>(id))?;
			let round = witness.get::<AESTowerField8b>(round)?;
			let multiples_16 = multiples_16.try_map(|id| witness.get::<AESTowerField8b>(id))?;

			round_consts_witness
				.par_iter_mut()
				.enumerate()
				.for_each(|(i, round_consts)| {
					(
						PackedType::<U, AESTowerField8b>::from_underliers_ref_mut(round_consts),
						input[8 * i].evals(),
						round.evals(),
						multiples_16[i].evals(),
					)
						.into_par_iter()
						.for_each(|(round_const, input, round, multiple16)| {
							*round_const = (*input) + (*round) + (*multiple16);
						});
				});
		}
		witness
			.set_owned::<AESTowerField8b, _>(std::iter::zip(round_consts, round_consts_witness))?;
	}
	Ok(round_consts)
}

/// Number of rounds in a Grøstl-256 compression
const N_ROUNDS: usize = 10;

const STATE_SIZE: usize = 64;

/// Constant vector of the Rijndael S-box affine transformation.
const SBOX_VEC: AESTowerField8b = AESTowerField8b::new(0x63);
/// Matrix columns of the Rijndael S-box affine transformation.
const SBOX_MATRIX: [AESTowerField8b; 8] = [
	AESTowerField8b::new(0b00011111),
	AESTowerField8b::new(0b00111110),
	AESTowerField8b::new(0b01111100),
	AESTowerField8b::new(0b11111000),
	AESTowerField8b::new(0b11110001),
	AESTowerField8b::new(0b11100011),
	AESTowerField8b::new(0b11000111),
	AESTowerField8b::new(0b10001111),
];
/// The first row of the circulant matrix defining the MixBytes step in Grøstl.
const MIX_BYTES_VEC: [AESTowerField8b; 8] = [
	AESTowerField8b::new(0x02),
	AESTowerField8b::new(0x02),
	AESTowerField8b::new(0x03),
	AESTowerField8b::new(0x04),
	AESTowerField8b::new(0x05),
	AESTowerField8b::new(0x03),
	AESTowerField8b::new(0x05),
	AESTowerField8b::new(0x07),
];

#[derive(Debug, Clone)]
struct MixColumn<F8b: Clone> {
	mix_bytes: [F8b; 8],
}

impl<F8b: Clone + From<AESTowerField8b>> Default for MixColumn<F8b> {
	fn default() -> Self {
		Self {
			mix_bytes: MIX_BYTES_VEC.map(F8b::from),
		}
	}
}

impl<F8b, P> CompositionPolyOS<P> for MixColumn<F8b>
where
	F8b: Field,
	P: PackedField<Scalar: ExtensionField<F8b>>,
{
	fn n_vars(&self) -> usize {
		9
	}

	fn degree(&self) -> usize {
		1
	}

	fn evaluate(&self, query: &[P]) -> Result<P, binius_math::Error> {
		if query.len() != 9 {
			return Err(binius_math::Error::IncorrectQuerySize { expected: 9 });
		}

		// This is unfortunate that it needs to unpack and repack...
		let result = iter::zip(query[1..].iter(), self.mix_bytes)
			.map(|(x_i, coeff)| P::from_fn(|j| x_i.get(j) * coeff))
			.sum::<P>();
		Ok(result - query[0])
	}

	fn binary_tower_level(&self) -> usize {
		AESTowerField8b::TOWER_LEVEL
	}
}

#[derive(Debug, Clone)]
struct SBoxConstraint;

impl<F, P> CompositionPolyOS<P> for SBoxConstraint
where
	F: TowerField,
	P: PackedField<Scalar = F>,
{
	fn n_vars(&self) -> usize {
		2
	}

	fn degree(&self) -> usize {
		3
	}

	fn evaluate(&self, query: &[P]) -> Result<P, binius_math::Error> {
		if query.len() != 2 {
			return Err(binius_math::Error::IncorrectQuerySize { expected: 2 });
		}

		let x = query[0];
		let inv = query[1];

		// x * inv == 1
		let non_zero_case = x * inv - F::ONE;

		// x == 0 AND inv == 0
		// TODO: Implement `mul_primitive` on packed tower fields
		let zero_case = x + P::from_fn(|i| {
			unsafe { inv.get_unchecked(i) }
				.mul_primitive(3)
				.expect("F must be tower height at least 4 by struct invariant")
		});

		// (x * inv == 1) OR (x == 0 AND inv == 0)
		Ok(non_zero_case * zero_case)
	}

	fn binary_tower_level(&self) -> usize {
		4
	}
}

fn s_box(x: AESTowerField8b) -> AESTowerField8b {
	#[rustfmt::skip]
	const S_BOX: [u8; 256] = [
		0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5,
		0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
		0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0,
		0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
		0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc,
		0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
		0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a,
		0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
		0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0,
		0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
		0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b,
		0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
		0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85,
		0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
		0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5,
		0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
		0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17,
		0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
		0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88,
		0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
		0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c,
		0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
		0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9,
		0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
		0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6,
		0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
		0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e,
		0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
		0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94,
		0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
		0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68,
		0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
	];
	let idx = u8::from(x) as usize;
	AESTowerField8b::from(S_BOX[idx])
}
