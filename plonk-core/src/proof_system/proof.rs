// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

//! A Proof stores the commitments to all of the elements that
//! are needed to univocally identify a prove of some statement.
//!
//! This module contains the implementation of the `StandardComposer`s
//! `Proof` structure and it's methods.

use crate::{
    commitment::HomomorphicCommitment,
    error::Error,
    label_commitment,
    proof_system::{
        ecc::{CurveAddition, FixedBaseScalarMul},
        linearisation_poly::ProofEvaluations,
        logic::Logic,
        range::Range,
        GateConstraint, VerifierKey as PlonkVerifierKey,
    },
    transcript::TranscriptProtocol,
    util::EvaluationDomainExt,
};
use ark_ec::TEModelParameters;

use ark_ff::{fields::batch_inversion, FftField, PrimeField};
use ark_poly::{EvaluationDomain, GeneralEvaluationDomain};
use ark_serialize::{
    CanonicalDeserialize, CanonicalSerialize, Read, SerializationError, Write,
};
use merlin::Transcript;

/// A Proof is a composition of `Commitment`s to the Witness, Permutation,
/// Quotient, Shifted and Opening polynomials as well as the
/// `ProofEvaluations`.
///
/// It's main goal is to allow the `Verifier` to
/// formally verify that the secret witnesses used to generate the [`Proof`]
/// satisfy a circuit that both [`Prover`](super::Prover) and
/// [`Verifier`](super::Verifier) have in common succintly and without any
/// capabilities of adquiring any kind of knowledge about the witness used to
/// construct the Proof.
#[derive(CanonicalDeserialize, CanonicalSerialize, derivative::Derivative)]
#[derivative(
    Clone(bound = "PC::Commitment: Clone, PC::Proof: Clone"),
    Debug(
        bound = "PC::Commitment: std::fmt::Debug, PC::Proof: std::fmt::Debug"
    ),
    Default(bound = "PC::Commitment: Default, PC::Proof: Default"),
    Eq(bound = "PC::Commitment: Eq, PC::Proof: Eq"),
    PartialEq(bound = "PC::Commitment: PartialEq, PC::Proof: PartialEq")
)]
pub struct Proof<F, PC>
where
    F: PrimeField,
    PC: HomomorphicCommitment<F>,
{
    /// Commitment to the witness polynomial for the left wires.
    pub(crate) a_comm: PC::Commitment,

    /// Commitment to the witness polynomial for the right wires.
    pub(crate) b_comm: PC::Commitment,

    /// Commitment to the witness polynomial for the output wires.
    pub(crate) c_comm: PC::Commitment,

    /// Commitment to the witness polynomial for the fourth wires.
    pub(crate) d_comm: PC::Commitment,

    /// Commitment to the permutation polynomial.
    pub(crate) z_comm: PC::Commitment,

    /// Commitment to the quotient polynomial.
    pub(crate) t_1_comm: PC::Commitment,

    /// Commitment to the quotient polynomial.
    pub(crate) t_2_comm: PC::Commitment,

    /// Commitment to the quotient polynomial.
    pub(crate) t_3_comm: PC::Commitment,

    /// Commitment to the quotient polynomial.
    pub(crate) t_4_comm: PC::Commitment,

    /// Batch opening proof of the aggregated witnesses
    pub aw_opening: PC::Proof,

    /// Batch opening proof of the shifted aggregated witnesses
    pub saw_opening: PC::Proof,

    /// Subset of all of the evaluations added to the proof.
    pub(crate) evaluations: ProofEvaluations<F>,
}

impl<F, PC> Proof<F, PC>
where
    F: PrimeField,
    PC: HomomorphicCommitment<F>,
{
    /// Performs the verification of a [`Proof`] returning a boolean result.
    pub(crate) fn verify<P>(
        &self,
        plonk_verifier_key: &PlonkVerifierKey<F, PC>,
        transcript: &mut Transcript,
        verifier_key: &PC::VerifierKey,
        pub_inputs: &[F],
    ) -> Result<(), Error>
    where
        P: TEModelParameters<BaseField = F>,
    {
        let domain =
            GeneralEvaluationDomain::<F>::new(plonk_verifier_key.n).ok_or(Error::InvalidEvalDomainSize {
                log_size_of_group: plonk_verifier_key.n.trailing_zeros(),
                adicity: <<F as FftField>::FftParams as ark_ff::FftParameters>::TWO_ADICITY,
            })?;

        // Subgroup checks are done when the proof is deserialised.

        // In order for the Verifier and Prover to have the same view in the
        // non-interactive setting Both parties must commit the same
        // elements into the transcript Below the verifier will simulate
        // an interaction with the prover by adding the same elements
        // that the prover added into the transcript, hence generating the
        // same challenges
        //
        // Add commitment to witness polynomials to transcript
        transcript.append(b"w_l", &self.a_comm);
        transcript.append(b"w_r", &self.b_comm);
        transcript.append(b"w_o", &self.c_comm);
        transcript.append(b"w_4", &self.d_comm);

        // Compute beta and gamma challenges
        let beta = transcript.challenge_scalar(b"beta");
        transcript.append(b"beta", &beta);
        let gamma = transcript.challenge_scalar(b"gamma");
        transcript.append(b"gamma", &gamma);

        assert!(beta != gamma, "challenges must be different");

        // Add commitment to permutation polynomial to transcript
        transcript.append(b"z", &self.z_comm);

        // Compute quotient challenge
        let alpha = transcript.challenge_scalar(b"alpha");
        let range_sep_challenge =
            transcript.challenge_scalar(b"range separation challenge");
        let logic_sep_challenge =
            transcript.challenge_scalar(b"logic separation challenge");
        let fixed_base_sep_challenge =
            transcript.challenge_scalar(b"fixed base separation challenge");
        let var_base_sep_challenge =
            transcript.challenge_scalar(b"variable base separation challenge");

        // Add commitment to quotient polynomial to transcript
        transcript.append(b"t_1", &self.t_1_comm);
        transcript.append(b"t_2", &self.t_2_comm);
        transcript.append(b"t_3", &self.t_3_comm);
        transcript.append(b"t_4", &self.t_4_comm);

        // Compute evaluation point challenge
        let z_challenge = transcript.challenge_scalar(b"z");

        // Compute zero polynomial evaluated at `z_challenge`
        let z_h_eval = domain.evaluate_vanishing_polynomial(z_challenge);

        // Compute first lagrange polynomial evaluated at `z_challenge`
        let l1_eval =
            compute_first_lagrange_evaluation(&domain, &z_h_eval, &z_challenge);

        let r0 = self.compute_r0(
            &domain,
            pub_inputs,
            alpha,
            beta,
            gamma,
            z_challenge,
            l1_eval,
            self.evaluations.perm_evals.permutation_eval,
        );

        // Add evaluations to transcript
        transcript.append(b"a_eval", &self.evaluations.wire_evals.a_eval);
        transcript.append(b"b_eval", &self.evaluations.wire_evals.b_eval);
        transcript.append(b"c_eval", &self.evaluations.wire_evals.c_eval);
        transcript.append(b"d_eval", &self.evaluations.wire_evals.d_eval);

        transcript.append(
            b"left_sig_eval",
            &self.evaluations.perm_evals.left_sigma_eval,
        );
        transcript.append(
            b"right_sig_eval",
            &self.evaluations.perm_evals.right_sigma_eval,
        );
        transcript.append(
            b"out_sig_eval",
            &self.evaluations.perm_evals.out_sigma_eval,
        );
        transcript.append(
            b"perm_eval",
            &self.evaluations.perm_evals.permutation_eval,
        );

        self.evaluations
            .custom_evals
            .vals
            .iter()
            .for_each(|(label, eval)| {
                let static_label = Box::leak(label.to_owned().into_boxed_str());
                transcript.append(static_label.as_bytes(), eval);
            });

        // Compute linearisation commitment
        let lin_comm = self.compute_linearisation_commitment::<P>(
            &domain,
            alpha,
            beta,
            gamma,
            range_sep_challenge,
            logic_sep_challenge,
            fixed_base_sep_challenge,
            var_base_sep_challenge,
            z_challenge,
            l1_eval,
            plonk_verifier_key,
        );

        // Commitment Scheme
        // Now we delegate computation to the commitment scheme by batch
        // checking two proofs.
        //
        // The `AggregateProof`, which proves that all the necessary
        // polynomials evaluated at `z_challenge` are
        // correct and a `Proof` which is proof that the
        // permutation polynomial evaluated at the shifted root of unity is
        // correct

        // Generation of the first aggregated proof: It ensures that the
        // polynomials evaluated at `z_challenge` are correct.

        // Reconstruct the Aggregated Proof commitments and evals
        // The proof consists of the witness commitment with no blinder

        // Compute aggregate witness to polynomials evaluated at the evaluation
        // challenge `z`
        let aw_challenge: F = transcript.challenge_scalar(b"aggregate_witness");

        let aw_commits = [
            label_commitment!(lin_comm),
            label_commitment!(plonk_verifier_key.permutation.left_sigma),
            label_commitment!(plonk_verifier_key.permutation.right_sigma),
            label_commitment!(plonk_verifier_key.permutation.out_sigma),
            label_commitment!(self.a_comm),
            label_commitment!(self.b_comm),
            label_commitment!(self.c_comm),
            label_commitment!(self.d_comm),
        ];

        let aw_evals = [
            -r0,
            self.evaluations.perm_evals.left_sigma_eval,
            self.evaluations.perm_evals.right_sigma_eval,
            self.evaluations.perm_evals.out_sigma_eval,
            self.evaluations.wire_evals.a_eval,
            self.evaluations.wire_evals.b_eval,
            self.evaluations.wire_evals.c_eval,
            self.evaluations.wire_evals.d_eval,
        ];

        let saw_challenge: F =
            transcript.challenge_scalar(b"aggregate_witness");

        let saw_commits = [
            label_commitment!(self.z_comm),
            label_commitment!(self.a_comm),
            label_commitment!(self.b_comm),
            label_commitment!(self.d_comm),
        ];

        let saw_evals = [
            self.evaluations.perm_evals.permutation_eval,
            self.evaluations.custom_evals.get("a_next_eval"),
            self.evaluations.custom_evals.get("b_next_eval"),
            self.evaluations.custom_evals.get("d_next_eval"),
        ];

        match PC::check(
            verifier_key,
            &aw_commits,
            &z_challenge,
            aw_evals,
            &self.aw_opening,
            aw_challenge,
            None,
        ) {
            Ok(true) => Ok(()),
            Ok(false) => Err(Error::ProofVerificationError),
            Err(e) => panic!("{:?}", e),
        }
        .and_then(|_| {
            match PC::check(
                verifier_key,
                &saw_commits,
                &(z_challenge * domain.element(1)),
                saw_evals,
                &self.saw_opening,
                saw_challenge,
                None,
            ) {
                Ok(true) => Ok(()),
                Ok(false) => Err(Error::ProofVerificationError),
                Err(e) => panic!("{:?}", e),
            }
        })
    }

    fn compute_r0(
        &self,
        domain: &GeneralEvaluationDomain<F>,
        pub_inputs: &[F],
        alpha: F,
        beta: F,
        gamma: F,
        z_challenge: F,
        l1_eval: F,
        z_hat_eval: F,
    ) -> F {
        // Compute the public input polynomial evaluated at `z_challenge`
        let pi_eval = compute_barycentric_eval(pub_inputs, z_challenge, domain);

        let alpha_sq = alpha.square();

        // a + beta * sigma_1 + gamma
        let beta_sig1 = beta * self.evaluations.perm_evals.left_sigma_eval;
        let b_0 = self.evaluations.wire_evals.a_eval + beta_sig1 + gamma;

        // b+ beta * sigma_2 + gamma
        let beta_sig2 = beta * self.evaluations.perm_evals.right_sigma_eval;
        let b_1 = self.evaluations.wire_evals.b_eval + beta_sig2 + gamma;

        // c+ beta * sigma_3 + gamma
        let beta_sig3 = beta * self.evaluations.perm_evals.out_sigma_eval;
        let b_2 = self.evaluations.wire_evals.c_eval + beta_sig3 + gamma;

        // ((d + gamma) * z_hat) * alpha
        let b_3 =
            (self.evaluations.wire_evals.d_eval + gamma) * z_hat_eval * alpha;

        let b = b_0 * b_1 * b_2 * b_3;

        // l_1(z) * alpha^2
        let c = l1_eval * alpha_sq;

        // Return r_0
        pi_eval - b - c
    }

    /// Computes the commitment to `[r]_1`.
    fn compute_linearisation_commitment<P>(
        &self,
        domain: &GeneralEvaluationDomain<F>,
        alpha: F,
        beta: F,
        gamma: F,
        range_sep_challenge: F,
        logic_sep_challenge: F,
        fixed_base_sep_challenge: F,
        var_base_sep_challenge: F,
        z_challenge: F,
        l1_eval: F,
        plonk_verifier_key: &PlonkVerifierKey<F, PC>,
    ) -> PC::Commitment
    where
        P: TEModelParameters<BaseField = F>,
    {
        // 5 for each type of gate + 1 for permutations + 4 for each piece of
        // the quotient poly
        let mut scalars = Vec::with_capacity(10);
        let mut points = Vec::with_capacity(10);

        plonk_verifier_key
            .arithmetic
            .compute_linearisation_commitment(
                &mut scalars,
                &mut points,
                &self.evaluations,
            );
        Range::extend_linearisation_commitment::<PC>(
            &plonk_verifier_key.range_selector_commitment,
            range_sep_challenge,
            &self.evaluations,
            &mut scalars,
            &mut points,
        );

        Logic::extend_linearisation_commitment::<PC>(
            &plonk_verifier_key.logic_selector_commitment,
            logic_sep_challenge,
            &self.evaluations,
            &mut scalars,
            &mut points,
        );

        FixedBaseScalarMul::<_, P>::extend_linearisation_commitment::<PC>(
            &plonk_verifier_key.fixed_group_add_selector_commitment,
            fixed_base_sep_challenge,
            &self.evaluations,
            &mut scalars,
            &mut points,
        );
        CurveAddition::<_, P>::extend_linearisation_commitment::<PC>(
            &plonk_verifier_key.variable_group_add_selector_commitment,
            var_base_sep_challenge,
            &self.evaluations,
            &mut scalars,
            &mut points,
        );
        plonk_verifier_key
            .permutation
            .compute_linearisation_commitment(
                &mut scalars,
                &mut points,
                &self.evaluations,
                z_challenge,
                (alpha, beta, gamma),
                l1_eval,
                self.z_comm.clone(),
            );

        // Second part
        let vanishing_poly_eval =
            domain.evaluate_vanishing_polynomial(z_challenge);
        // z_challenge ^ n
        let z_challenge_to_n = vanishing_poly_eval + F::one();

        let t_1_scalar = -vanishing_poly_eval;
        let t_2_scalar = t_1_scalar * z_challenge_to_n;
        let t_3_scalar = t_2_scalar * z_challenge_to_n;
        let t_4_scalar = t_3_scalar * z_challenge_to_n;
        scalars.extend_from_slice(&[
            t_1_scalar, t_2_scalar, t_3_scalar, t_4_scalar,
        ]);
        points.extend_from_slice(&[
            self.t_1_comm.clone(),
            self.t_2_comm.clone(),
            self.t_3_comm.clone(),
            self.t_4_comm.clone(),
        ]);

        PC::multi_scalar_mul(&points, &scalars)
    }
}

/// The first lagrange polynomial has the expression:
///
/// ```text
/// L_0(X) = mul_from_1_to_(n-1) [(X - omega^i) / (1 - omega^i)]
/// ```
///
/// with `omega` being the generator of the domain (the `n`th root of unity).
///
/// We use two equalities:
///   1. `mul_from_2_to_(n-1) [1 / (1 - omega^i)] = 1 / n`
///   2. `mul_from_2_to_(n-1) [(X - omega^i)] = (X^n - 1) / (X - 1)`
/// to obtain the expression:
///
/// ```text
/// L_0(X) = (X^n - 1) / n * (X - 1)
/// ```
fn compute_first_lagrange_evaluation<F>(
    domain: &GeneralEvaluationDomain<F>,
    z_h_eval: &F,
    z_challenge: &F,
) -> F
where
    F: PrimeField,
{
    let n_fr = F::from(domain.size() as u64);
    let denom = n_fr * (*z_challenge - F::one());
    *z_h_eval * denom.inverse().unwrap()
}

fn compute_barycentric_eval<F>(
    evaluations: &[F],
    point: F,
    domain: &GeneralEvaluationDomain<F>,
) -> F
where
    F: PrimeField,
{
    let numerator =
        domain.evaluate_vanishing_polynomial(point) * domain.size_inv();
    let range = 0..evaluations.len();

    let non_zero_evaluations = range
        .filter(|&i| {
            let evaluation = &evaluations[i];
            evaluation != &F::zero()
        })
        .collect::<Vec<_>>();

    // Only compute the denominators with non-zero evaluations
    let range = 0..non_zero_evaluations.len();

    let group_gen_inv = domain.group_gen_inv();
    let mut denominators = range
        .clone()
        .map(|i| {
            // index of non-zero evaluation
            let index = non_zero_evaluations[i];
            (group_gen_inv.pow(&[index as u64, 0, 0, 0]) * point) - F::one()
        })
        .collect::<Vec<_>>();
    batch_inversion(&mut denominators);

    let result: F = range
        .map(|i| {
            let eval_index = non_zero_evaluations[i];
            let eval = evaluations[eval_index];
            denominators[i] * eval
        })
        .sum();

    result * numerator
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::batch_test_kzg;
    use ark_bls12_377::Bls12_377;
    use ark_bls12_381::Bls12_381;

    fn test_serde_proof<F, P, PC>()
    where
        F: PrimeField,
        P: TEModelParameters<BaseField = F>,
        PC: HomomorphicCommitment<F>,
        Proof<F, PC>: std::fmt::Debug + PartialEq,
    {
        let proof =
            crate::constraint_system::helper::gadget_tester::<F, P, PC>(
                |_: &mut crate::constraint_system::StandardComposer<F, P>| {},
                200,
            )
            .expect("Empty circuit failed");

        let mut proof_bytes = vec![];
        proof.serialize(&mut proof_bytes).unwrap();

        let obtained_proof =
            Proof::<F, PC>::deserialize(proof_bytes.as_slice()).unwrap();

        assert_eq!(proof, obtained_proof);
    }

    // Bls12-381 tests
    batch_test_kzg!(
        [test_serde_proof],
        [] => (
            Bls12_381, ark_ed_on_bls12_381::EdwardsParameters
        )
    );
    // Bls12-377 tests
    batch_test_kzg!(
        [test_serde_proof],
        [] => (
            Bls12_377, ark_ed_on_bls12_377::EdwardsParameters
        )
    );
}
