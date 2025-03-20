use super::framework::TestFramework;
use crate::circuits::{
    constraints::ConstraintSystem,
    gate::CircuitGate,
    polynomial::COLUMNS,
    polynomials::{generic::GenericGateSpec, xor},
    wires::Wire,
};
use ark_ff::Zero;
use itertools::iterate;
use mina_curves::pasta::{Fp, Vesta, VestaParameters};
use mina_poseidon::{
    constants::PlonkSpongeConstantsKimchi,
    sponge::{DefaultFqSponge, DefaultFrSponge},
};
use rand::Rng;
use std::array;

type SpongeParams = PlonkSpongeConstantsKimchi;
type BaseSponge = DefaultFqSponge<VestaParameters, SpongeParams>;
type ScalarSponge = DefaultFrSponge<Fp, SpongeParams>;

#[test]
fn test_lazy_mode_benchmark() {
    let public = vec![Fp::from(1u8); 5];
    let circuit_size = 1 << 16;

    let mut gates_row = iterate(0, |&i| i + 1);
    let mut gates = Vec::with_capacity(circuit_size);
    let mut witness: [Vec<Fp>; COLUMNS] = array::from_fn(|_| vec![Fp::zero(); circuit_size]);

    let rng = &mut o1_utils::tests::make_test_rng(None);

    // public input
    for p in public.iter() {
        let r = gates_row.next().unwrap();
        witness[0][r] = *p;
        gates.push(CircuitGate::create_generic_gadget(
            Wire::for_row(r),
            GenericGateSpec::Pub,
            None,
        ));
    }

    let bits = 64;

    while gates.len() < circuit_size - 5 {
        CircuitGate::<Fp>::extend_xor_gadget(&mut gates, bits);

        let input1 = Fp::from(rng.gen_range(0u64..1 << (bits - 1)));
        let input2 = Fp::from(rng.gen_range(0u64..1 << (bits - 1)));

        xor::extend_xor_witness(&mut witness, input1, input2, bits);
    }

    {
        // LAZY CACHE FALSE
        eprintln!("LAZY CACHE: false (default)");
        let gates_ = gates.clone();
        let witness_ = witness.clone();
        let public_ = public.clone();
        TestFramework::<Vesta>::default()
            .gates(gates_)
            .witness(witness_)
            .public_inputs(public_)
            .lazy_mode(false) // optional, default is false
            .setup()
            .prove_and_verify::<BaseSponge, ScalarSponge>()
            .unwrap();
    }

    {
        // LAZY CACHE TRUE
        eprintln!("LAZY CACHE: true");
        TestFramework::<Vesta>::default()
            .gates(gates)
            .witness(witness)
            .public_inputs(public)
            .lazy_mode(true)
            .setup()
            .prove_and_verify::<BaseSponge, ScalarSponge>()
            .unwrap();
    }
}
