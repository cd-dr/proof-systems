use algebra::{
    pasta::{
        fp::Fp,
        pallas::Affine as Other,
        vesta::{Affine, VestaParameters},
    },
    Field, One, UniformRand, Zero,
};
use array_init::array_init;
use colored::Colorize;
use commitment_dlog::{
    commitment::{b_poly_coefficients, ceil_log2, CommitmentCurve},
    srs::{endos, SRS},
};
use ff_fft::DensePolynomial;
use groupmap::GroupMap;
use oracle::{
    poseidon::{ArithmeticSponge, Plonk15SpongeConstants, Sponge, SpongeConstants},
    sponge::{DefaultFqSponge, DefaultFrSponge},
};
use plonk_15_wires_circuits::wires::{Wire, COLUMNS};
use plonk_15_wires_circuits::{
    gate::CircuitGate,
    gates::poseidon::{round_range, ROUNDS_PER_ROW, SPONGE_WIDTH},
    nolookup::constraints::ConstraintSystem,
};
use plonk_15_wires_protocol_dlog::{
    index::{Index, SRSSpec},
    prover::ProverProof,
};
use rand::{rngs::StdRng, SeedableRng};
use std::time::Instant;
use std::{io, io::Write};

// aliases

type SpongeParams = Plonk15SpongeConstants;
type BaseSponge = DefaultFqSponge<VestaParameters, SpongeParams>;
type ScalarSponge = DefaultFrSponge<Fp, SpongeParams>;

// const PERIOD: usize = SpongeParams::ROUNDS_FULL + 1;
// const M: usize = PERIOD * (NUM_POS-1);
// const MAX_SIZE: usize = N; // max size of poly chunks
const PUBLIC: usize = 0;

const NUM_POS: usize = 1; // 1360; // number of Poseidon hashes in the circuit
const ROUNDS_PER_HASH: usize = SpongeParams::ROUNDS_FULL;
const POS_ROWS_PER_HASH: usize = ROUNDS_PER_HASH / ROUNDS_PER_ROW;
const N_LOWER_BOUND: usize = (POS_ROWS_PER_HASH + 1) * NUM_POS; // Plonk domain size

#[test]
fn poseidon_vesta_15_wires() {
    let max_size = 1 << ceil_log2(N_LOWER_BOUND);
    println!("max_size = {}", max_size);
    println!("rounds per hash = {}", ROUNDS_PER_HASH);
    println!("rounds per row = {}", ROUNDS_PER_ROW);
    assert_eq!(ROUNDS_PER_HASH % ROUNDS_PER_ROW, 0);

    let round_constants = &oracle::pasta::fp::params().round_constants;

    // we keep track of an absolute row, and relative row within a gadget
    let mut abs_row = 0;

    // circuit gates
    let mut gates: Vec<CircuitGate<Fp>> = Vec::with_capacity(max_size);

    // custom constraints for Poseidon hash function permutation
    // ROUNDS_FULL full rounds constraint gates
    for _ in 0..NUM_POS {
        // create a poseidon gadget manully
        for rel_row in 0..POS_ROWS_PER_HASH {
            // the 15 wires for this row
            let wires = array_init(|col| Wire { col, row: abs_row });

            // round constant for this row
            let coeffs = array_init(|offset| {
                let round = rel_row * ROUNDS_PER_ROW + offset + 1;
                array_init(|field_el| round_constants[round][field_el])
            });

            // create poseidon gate for this row
            gates.push(CircuitGate::<Fp>::create_poseidon(abs_row, wires, coeffs));
            abs_row += 1;
        }

        // final (zero) gate that contains the output of poseidon
        let wires = array_init(|col| Wire { col, row: abs_row });
        gates.push(CircuitGate::<Fp>::zero(abs_row, wires));
        abs_row += 1;
    }

    /*
    for j in 0..SpongeParams::ROUNDS_FULL-2
    {
        gates.push(CircuitGate::<Fp>::create_poseidon(i, [Wire{col:0, row:i}, Wire{col:1, row:i}, Wire{col:2, row:i}, Wire{col:3, row:i}, Wire{col:4, row:i}], round_constants[j].clone()));
        i+=1;
    }
    gates.push(CircuitGate::<Fp>::zero(i, [Wire{col:0, row:i}, Wire{col:1, row:i}, Wire{col:2, row:i}, Wire{col:3, row:i}, Wire{col:4, row:i}]));
    i+=1;
    gates.push(CircuitGate::<Fp>::zero(i, [Wire{col:0, row:i}, Wire{col:1, row:i}, Wire{col:2, row:i}, Wire{col:3, row:i}, Wire{col:4, row:i}]));
    i+=1;
    gates.push(CircuitGate::<Fp>::zero(i, [Wire{col:0, row:i}, Wire{col:1, row:i}, Wire{col:2, row:i}, Wire{col:3, row:i}, Wire{col:4, row:i}]));
    */

    // create the index
    let fp_sponge_params = oracle::pasta::fp::params();
    let cs = ConstraintSystem::<Fp>::create(gates, fp_sponge_params, PUBLIC).unwrap();
    let fq_sponge_params = oracle::pasta::fq::params();
    let (endo_q, _endo_r) = endos::<Other>();
    let srs = SRS::create(max_size);
    let srs = SRSSpec::Use(&srs);

    let index = Index::<Affine>::create(cs, fq_sponge_params, endo_q, srs);

    positive(&index);
}

// creates a proof and verifies it
fn positive(index: &Index<Affine>) {
    // constant
    let max_size = 1 << ceil_log2(N_LOWER_BOUND);

    // set up
    let rng = &mut StdRng::from_seed([0u8; 32]);
    let params = oracle::pasta::fp::params();
    let mut sponge = ArithmeticSponge::<Fp, SpongeParams>::new();
    let group_map = <Affine as CommitmentCurve>::Map::setup();

    // batching what?
    let mut batch = Vec::new();

    // debug
    println!("{}{:?}", "Circuit size: ".yellow(), max_size);
    println!("{}{:?}", "Polycommitment chunk size: ".yellow(), max_size);
    println!(
        "{}{:?}",
        "Number oh Poseidon hashes in the circuit: ".yellow(),
        NUM_POS
    );
    println!(
        "{}{:?}",
        "Full rounds: ".yellow(),
        SpongeParams::ROUNDS_FULL
    );
    println!("{}{:?}", "Sbox alpha: ".yellow(), SpongeParams::SPONGE_BOX);
    println!("{}", "Base curve: vesta\n".green());
    println!("{}", "Prover zk-proof computation".green());

    let mut start = Instant::now();

    // why would there be several tests?
    for test in 0..1 {
        // witness for Poseidon permutation custom constraints
        // (15 columns of vectors, each vector is of size n)
        let mut witness: [Vec<Fp>; COLUMNS] = array_init(|_| vec![Fp::zero(); max_size]);
        println!("witness: {:?}", witness);

        // creates a random initial state
        let init = vec![
            Fp::rand(rng),
            Fp::rand(rng),
            Fp::rand(rng),
            Fp::rand(rng),
            Fp::rand(rng),
        ];

        // number of poseidon instances in the circuit
        for h in 0..NUM_POS {
            // index
            // TODO: is the `+ 1` correct?
            let first_row = h * (POS_ROWS_PER_HASH + 1);

            // initialize the sponge in the circuit with our random state
            let first_state_cols = &mut witness[round_range(0)];
            for state_idx in 0..SPONGE_WIDTH {
                first_state_cols[state_idx][first_row] = init[state_idx];
            }

            // set the sponge state
            sponge.state = init.clone();

            // for the poseidon rows
            for row_idx in 0..POS_ROWS_PER_HASH {
                let row = row_idx + first_row;
                for round in 0..ROUNDS_PER_ROW {
                    // the last round makes use of the next row
                    let maybe_next_row = if round == ROUNDS_PER_ROW - 1 {
                        row + 1
                    } else {
                        row
                    };

                    //
                    let abs_round = round + row_idx * ROUNDS_PER_ROW;

                    // TODO: this won't work if the circuit has an INITIAL_ARK
                    sponge.full_round(abs_round, &params);

                    // apply the sponge and record the result in the witness
                    let cols_to_update = round_range((round + 1) % ROUNDS_PER_ROW);
                    witness[cols_to_update]
                        .iter_mut()
                        .zip(sponge.state.iter())
                        // update the state (last update is on the next row)
                        .for_each(|(w, s)| w[maybe_next_row] = *s);
                }
            }
        }

        /*
        sponge.state = init.clone();
        w.iter_mut().zip(sponge.state.iter()).for_each(|(w, s)| w.push(*s));

        // ROUNDS_FULL full rounds
        for j in 0..SpongeParams::ROUNDS_FULL-2
        {
            sponge.full_round(j, &params);
            w.iter_mut().zip(sponge.state.iter()).for_each(|(w, s)| w.push(*s));
        }

        w.iter_mut().for_each(|w| {w.push(Fp::rand(rng)); w.push(Fp::rand(rng))}); */

        // verify the circuit satisfiability by the computed witness
        assert_eq!(index.cs.verify(&witness), true);

        // what is this thing?
        let prev = {
            // ?
            let k = ceil_log2(index.srs.get_ref().g.len());

            // random challenges
            let chals: Vec<_> = (0..k).map(|_| Fp::rand(rng)).collect();

            // non-hiding commitments of b polyonmials (made out of challenges) ???
            let comm = {
                let coeffs = b_poly_coefficients(&chals);
                let b = DensePolynomial::from_coefficients_vec(coeffs);
                index.srs.get_ref().commit_non_hiding(&b, None)
            };

            (chals, comm)
        };

        println!("n vs domain{} {}", max_size, index.cs.domain.d1.size);

        // add the proof to the batch
        // TODO: create and verify should not take group_map, that should be during an init phase
        batch.push(
            ProverProof::create::<BaseSponge, ScalarSponge>(
                &group_map,
                &witness,
                &index,
                vec![prev],
            )
            .unwrap(),
        );

        print!("{:?}\r", test);
        io::stdout().flush().unwrap();
    }
    // TODO: this should move to a bench
    println!("{}{:?}", "Execution time: ".yellow(), start.elapsed());

    // TODO: shouldn't verifier_index be part of ProverProof, not being passed in verify?
    let verifier_index = index.verifier_index();

    let lgr_comms = vec![]; // why empty?
    let batch: Vec<_> = batch
        .iter()
        .map(|proof| (&verifier_index, &lgr_comms, proof))
        .collect();

    // verify the proofs in batch
    println!("{}", "Verifier zk-proofs verification".green());
    start = Instant::now();
    match ProverProof::verify::<BaseSponge, ScalarSponge>(&group_map, &batch) {
        Err(error) => panic!("Failure verifying the prover's proofs in batch: {}", error),
        Ok(_) => {
            println!("{}{:?}", "Execution time: ".yellow(), start.elapsed());
        }
    }
}

#[test]
fn generic_vesta_15_wires() {
    // circuit gates
    let mut gates = vec![];
    let mut abs_row = 0;

    // add generic gate for multiplication l * r = o
    let wires = array_init(|col| Wire { col, row: abs_row });
    let (on, off) = (Fp::one(), Fp::zero());
    let qw: [Fp; COLUMNS] = [
        /* left for addition */ off, /* right for addition */ off, /* output */ on,
        /* the rest of the columns don't matter */
        off, off, off, off, off, off, off, off, off, off, off, off,
    ];
    let multiplication = on;
    let constant = off;
    gates.push(CircuitGate::<Fp>::create_generic(
        abs_row,
        wires,
        qw,
        multiplication,
        constant,
    ));
    abs_row += 1;

    // add a zero gate, just because
    let wires = array_init(|col| Wire { col, row: abs_row });
    gates.push(CircuitGate::<Fp>::zero(abs_row, wires));
    abs_row += 1;

    // create the index
    let fp_sponge_params = oracle::pasta::fp::params();
    let public = 0;
    let cs = ConstraintSystem::<Fp>::create(gates, fp_sponge_params, public).unwrap();
    let n = cs.domain.d1.size as usize;
    let fq_sponge_params = oracle::pasta::fq::params();
    let (endo_q, _endo_r) = endos::<Other>();
    let srs = SRS::create(abs_row);
    let srs = SRSSpec::Use(&srs);
    let index = Index::<Affine>::create(cs, fq_sponge_params, endo_q, srs);

    // set up
    let rng = &mut StdRng::from_seed([0u8; 32]);
    let group_map = <Affine as CommitmentCurve>::Map::setup();

    // create witness
    let mut witness: [Vec<Fp>; COLUMNS] = array_init(|_| vec![Fp::zero(); n]);
    let left = 0;
    let right = 1;
    let output = 2;

    // fill witness
    let mut row = 0;
    witness[left][row] = 3u32.into();
    witness[right][row] = 5u32.into();
    witness[output][row] = -Fp::from(3u32 * 5);
    row += 1;
    println!("witness: {:?}", witness);

    // zero gate
    row += 1;

    //
    assert_eq!(row, abs_row);
    println!("{}", "witness has been filled".green());

    // verify the circuit satisfiability by the computed witness
    assert_eq!(index.cs.verify(&witness), true);
    println!("{}", "circuit is fine".green());

    // previous opening for recursion
    let prev = {
        let k = ceil_log2(index.srs.get_ref().g.len());
        let chals: Vec<_> = (0..k).map(|_| Fp::rand(rng)).collect();
        let comm = {
            let coeffs = b_poly_coefficients(&chals);
            let b = DensePolynomial::from_coefficients_vec(coeffs);
            index.srs.get_ref().commit_non_hiding(&b, None)
        };
        (chals, comm)
    };

    // add the proof to the batch
    let mut batch = Vec::new();
    batch.push(
        ProverProof::create::<BaseSponge, ScalarSponge>(&group_map, &witness, &index, vec![prev])
            .unwrap(),
    );
    println!("{}", "proof created".green());

    // verify the proof
    let verifier_index = index.verifier_index();
    let lgr_comms = vec![]; // why empty?
    let batch: Vec<_> = batch
        .iter()
        .map(|proof| (&verifier_index, &lgr_comms, proof))
        .collect();
    match ProverProof::verify::<BaseSponge, ScalarSponge>(&group_map, &batch) {
        Err(error) => panic!("error: {}", error),
        Ok(_) => {
            println!("{}", "proof verified".green());
        }
    }
}