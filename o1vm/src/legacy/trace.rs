//! This module defines structures and traits to build and manipulate traces.
//! A trace is a collection of data points that represent the execution of a
//! program.
//! Some trace can be seen as "decomposable" in the sense that they can be
//! divided into sub-traces that share the same columns, and sub-traces can be
//! selected using "selectors".

use crate::{
    legacy::{
        folding::{BaseField, FoldingInstance, FoldingWitness, ScalarField},
        Curve, Pairing,
    },
    lookups::Lookup,
    E,
};
use ark_ff::{One, Zero};
use ark_poly::{Evaluations, Radix2EvaluationDomain as D};
use folding::{expressions::FoldingCompatibleExpr, Alphas, FoldingConfig};
use itertools::Itertools;
use kimchi::circuits::berkeley_columns::BerkeleyChallengeTerm;
use kimchi_msm::{columns::Column, witness::Witness};
use mina_poseidon::sponge::FqSponge;
use poly_commitment::{commitment::absorb_commitment, PolyComm, SRS as _};
use rayon::{iter::ParallelIterator, prelude::IntoParallelIterator};
use std::{collections::BTreeMap, ops::Index};

/// Implement a trace for a single instruction.
// TODO: we should use the generic traits defined in [kimchi_msm].
// For now, we want to have this to be able to test the folding library for a
// single instruction.
// It is not recommended to use this in production and it should not be
// maintained in the long term.
#[derive(Clone)]
pub struct Trace<const N: usize, C: FoldingConfig> {
    pub domain_size: usize,
    pub witness: Witness<N, Vec<ScalarField<C>>>,
    pub constraints: Vec<E<ScalarField<C>>>,
    pub lookups: Vec<Lookup<E<ScalarField<C>>>>,
}

/// Struct representing a circuit execution trace which is decomposable in
/// individual sub-circuits sharing the same columns.
/// It is parameterized by
/// - `N`: the total number of columns (constant), it must equal `N_REL + N_DSEL`
/// - `N_REL`: the number of relation columns (constant),
/// - `N_DSEL`: the number of dynamic selector columns (constant),
/// - `Selector`: an enum representing the different gate behaviours,
/// - `F`: the type of the witness data.
#[derive(Clone)]
pub struct DecomposedTrace<const N: usize, C: FoldingConfig> {
    /// The domain size of the circuit (should coincide with that of the traces)
    pub domain_size: usize,
    /// The traces are indexed by the selector
    /// Inside the witness of the trace for a given selector,
    /// - the last N_SEL columns represent the selector columns
    ///   and only the one for `Selector` should be all ones (the rest of selector columns should be all zeros)
    pub trace: BTreeMap<C::Selector, Trace<N, C>>,
}

// Implementation of [Index] using `C::Selector`` as the index for [DecomposedTrace] to access the trace directly.
impl<const N: usize, C: FoldingConfig> Index<C::Selector> for DecomposedTrace<N, C> {
    type Output = Trace<N, C>;

    fn index(&self, index: C::Selector) -> &Self::Output {
        &self.trace[&index]
    }
}

impl<const N: usize, C: FoldingConfig> DecomposedTrace<N, C>
where
    usize: From<<C as FoldingConfig>::Selector>,
{
    /// Returns the number of rows that have been instantiated for the given
    /// selector.
    /// It is important that the column used is a relation column because
    /// selector columns are only instantiated at the very end, so their length
    /// could be zero most times.
    /// That is the reason that relation columns are located first.
    pub fn number_of_rows(&self, opcode: C::Selector) -> usize {
        self[opcode].witness.cols[0].len()
    }

    /// Returns a boolean indicating whether the witness for the given selector
    /// was ever found in the circuit or not.
    pub fn in_circuit(&self, opcode: C::Selector) -> bool {
        self.number_of_rows(opcode) != 0
    }

    /// Returns whether the witness for the given selector has achieved a number
    /// of rows that is equal to the domain size.
    pub fn is_full(&self, opcode: C::Selector) -> bool {
        self.domain_size == self.number_of_rows(opcode)
    }

    /// Resets the witness after folding
    pub fn reset(&mut self, opcode: C::Selector) {
        (self.trace.get_mut(&opcode).unwrap().witness.cols.as_mut())
            .iter_mut()
            .for_each(Vec::clear);
    }

    /// Sets the selector column to all ones, and the rest to all zeros
    pub fn set_selector_column<const N_REL: usize>(
        &mut self,
        selector: C::Selector,
        number_of_rows: usize,
    ) {
        (N_REL..N).for_each(|i| {
            if i == usize::from(selector) {
                self.trace.get_mut(&selector).unwrap().witness.cols[i]
                    .extend((0..number_of_rows).map(|_| ScalarField::<C>::one()))
            } else {
                self.trace.get_mut(&selector).unwrap().witness.cols[i]
                    .extend((0..number_of_rows).map(|_| ScalarField::<C>::zero()))
            }
        });
    }
}

/// The trait [Foldable] describes structures that can be folded.
/// For that, it requires to be able to implement a way to return a folding
/// instance and a folding witness.
/// It is specialized for the [DecomposedTrace] struct for now and is expected
/// to fold individual instructions, selected with a specific `C::Selector`.
pub trait Foldable<const N: usize, C: FoldingConfig, Sponge> {
    /// Returns the witness for the given selector as a folding witness and
    /// folding instance pair.
    /// Note that this function will also absorb all commitments to the columns
    /// to coin challenges appropriately.
    fn to_folding_pair(
        &self,
        selector: C::Selector,
        fq_sponge: &mut Sponge,
        domain: D<ScalarField<C>>,
        srs: &poly_commitment::kzg::PairingSRS<Pairing>,
    ) -> (
        FoldingInstance<N, C::Curve>,
        FoldingWitness<N, ScalarField<C>>,
    );

    /// Returns a map of constraints that are compatible with folding for each selector
    fn folding_constraints(&self) -> BTreeMap<C::Selector, Vec<FoldingCompatibleExpr<C>>>;
}

/// Implement the trait Foldable for the structure [DecomposedTrace]
impl<const N: usize, C: FoldingConfig<Column = Column, Curve = Curve>, Sponge>
    Foldable<N, C, Sponge> for DecomposedTrace<N, C>
where
    C::Selector: Into<usize>,
    Sponge: FqSponge<BaseField<C>, C::Curve, ScalarField<C>>,
    <C as FoldingConfig>::Challenge: From<BerkeleyChallengeTerm>,
{
    fn to_folding_pair(
        &self,
        selector: C::Selector,
        fq_sponge: &mut Sponge,
        domain: D<ScalarField<C>>,
        srs: &poly_commitment::kzg::PairingSRS<Pairing>,
    ) -> (
        FoldingInstance<N, C::Curve>,
        FoldingWitness<N, ScalarField<C>>,
    ) {
        let folding_witness = FoldingWitness {
            witness: (&self[selector].witness)
                .into_par_iter()
                .map(|w| Evaluations::from_vec_and_domain(w.to_vec(), domain))
                .collect(),
        };

        let commitments: Witness<N, PolyComm<C::Curve>> = (&folding_witness.witness)
            .into_par_iter()
            .map(|w| srs.commit_evaluations_non_hiding(domain, w))
            .collect();

        // Absorbing commitments
        (&commitments)
            .into_iter()
            .for_each(|c| absorb_commitment(fq_sponge, c));

        let commitments: [C::Curve; N] = commitments
            .into_iter()
            .map(|c| c.get_first_chunk())
            .collect_vec()
            .try_into()
            .unwrap();

        let beta = fq_sponge.challenge();
        let gamma = fq_sponge.challenge();
        let joint_combiner = fq_sponge.challenge();
        let alpha = fq_sponge.challenge();
        let challenges = [beta, gamma, joint_combiner];
        let alphas = Alphas::new(alpha);
        let blinder = ScalarField::<C>::one();
        let instance = FoldingInstance {
            commitments,
            challenges,
            alphas,
            blinder,
        };

        (instance, folding_witness)
    }

    fn folding_constraints(&self) -> BTreeMap<C::Selector, Vec<FoldingCompatibleExpr<C>>> {
        self.trace
            .iter()
            .map(|(k, instr)| {
                (
                    *k,
                    instr
                        .constraints
                        .iter()
                        .map(|x| FoldingCompatibleExpr::from(x.clone()))
                        .collect(),
                )
            })
            .collect()
    }
}

/// Tracer builds traces for some program executions.
/// The constant type `N_REL` is defined as the maximum number of relation
/// columns the trace can use per row.
/// The type `C` encodes the folding configuration, from which the selector,
/// which encodes the information of the kind of information the trace encodes,
/// and scalar field are derived. Examples of selectors are:
/// - For Keccak, `Step` encodes the row being performed at a time: round,
/// squeeze, padding, etc...
/// - For MIPS, `Instruction` encodes the CPU instruction being executed: add,
/// sub, load, store, etc...
pub trait Tracer<const N_REL: usize, C: FoldingConfig, Env> {
    type Selector;

    /// Initialize a new trace with the given domain size, selector, and environment.
    fn init(domain_size: usize, selector: C::Selector, env: &mut Env) -> Self;

    /// Add a witness row to the circuit (only for relation columns)
    fn push_row(&mut self, selector: Self::Selector, row: &[ScalarField<C>; N_REL]);

    /// Pad the rows of one opcode with the given row until
    /// reaching the domain size if needed.
    /// Returns the number of rows that were added.
    /// It does not add selector columns.
    fn pad_with_row(&mut self, selector: Self::Selector, row: &[ScalarField<C>; N_REL]) -> usize;

    /// Pads the rows of one opcode with zero rows until
    /// reaching the domain size if needed.
    /// Returns the number of rows that were added.
    /// It does not add selector columns.
    fn pad_with_zeros(&mut self, selector: Self::Selector) -> usize;

    /// Pad the rows of one opcode with the first row until
    /// reaching the domain size if needed.
    /// It only tries to pad witnesses which are non empty.
    /// Returns the number of rows that were added.
    /// It does not add selector columns.
    /// - Use `None` for single traces
    /// - Use `Some(selector)` for multi traces
    fn pad_dummy(&mut self, selector: Self::Selector) -> usize;
}

/// DecomposableTracer builds traces for some program executions.
/// The constant type `N_REL` is defined as the maximum number of relation
/// columns the trace can use per row.
/// The type `C` encodes the folding configuration, from which the selector,
/// and scalar field are derived. Examples of selectors are:
/// - For Keccak, `Step` encodes the row being performed at a time: round,
/// squeeze, padding, etc...
/// - For MIPS, `Instruction` encodes the CPU instruction being executed: add,
/// sub, load, store, etc...
pub trait DecomposableTracer<Env> {
    /// Create a new decomposable trace with the given domain size, and environment.
    fn new(domain_size: usize, env: &mut Env) -> Self;

    /// Pads the rows of the witnesses until reaching the domain size using the first
    /// row repeatedly. It does not add selector columns.
    fn pad_witnesses(&mut self);
}

/// Generic implementation of the [Tracer] trait for the [DecomposedTrace] struct.
/// It requires the [DecomposedTrace] to implement the [DecomposableTracer] trait,
/// and the [Trace] struct to implement the [Tracer] trait with Selector set to (),
/// and `usize` to implement the [From] trait with `C::Selector`.
impl<const N: usize, const N_REL: usize, C: FoldingConfig, Env> Tracer<N_REL, C, Env>
    for DecomposedTrace<N, C>
where
    DecomposedTrace<N, C>: DecomposableTracer<Env>,
    Trace<N, C>: Tracer<N_REL, C, Env, Selector = ()>,
    usize: From<<C as FoldingConfig>::Selector>,
{
    type Selector = C::Selector;

    fn init(domain_size: usize, _selector: C::Selector, env: &mut Env) -> Self {
        <Self as DecomposableTracer<Env>>::new(domain_size, env)
    }

    fn push_row(&mut self, selector: Self::Selector, row: &[ScalarField<C>; N_REL]) {
        self.trace.get_mut(&selector).unwrap().push_row((), row);
    }

    fn pad_with_row(&mut self, selector: Self::Selector, row: &[ScalarField<C>; N_REL]) -> usize {
        // We only want to pad non-empty witnesses.
        if !self.in_circuit(selector) {
            0
        } else {
            self.trace.get_mut(&selector).unwrap().pad_with_row((), row)
        }
    }

    fn pad_with_zeros(&mut self, selector: Self::Selector) -> usize {
        // We only want to pad non-empty witnesses.
        if !self.in_circuit(selector) {
            0
        } else {
            self.trace.get_mut(&selector).unwrap().pad_with_zeros(())
        }
    }

    fn pad_dummy(&mut self, selector: Self::Selector) -> usize {
        // We only want to pad non-empty witnesses.
        if !self.in_circuit(selector) {
            0
        } else {
            self.trace.get_mut(&selector).unwrap().pad_dummy(())
        }
    }
}

pub mod keccak {
    use std::{array, collections::BTreeMap};

    use ark_ff::Zero;
    use kimchi_msm::witness::Witness;
    use strum::IntoEnumIterator;

    use crate::{
        interpreters::keccak::{
            column::{Steps, N_ZKVM_KECCAK_COLS, N_ZKVM_KECCAK_REL_COLS},
            environment::KeccakEnv,
            standardize,
        },
        legacy::{
            folding::{keccak::KeccakConfig, ScalarField},
            trace::{DecomposableTracer, DecomposedTrace, Trace, Tracer},
        },
    };

    /// A Keccak instruction trace
    pub type KeccakTrace = Trace<N_ZKVM_KECCAK_COLS, KeccakConfig>;
    /// The Keccak circuit trace
    pub type DecomposedKeccakTrace = DecomposedTrace<N_ZKVM_KECCAK_COLS, KeccakConfig>;

    impl DecomposableTracer<KeccakEnv<ScalarField<KeccakConfig>>> for DecomposedKeccakTrace {
        fn new(domain_size: usize, env: &mut KeccakEnv<ScalarField<KeccakConfig>>) -> Self {
            let mut circuit = Self {
                domain_size,
                trace: BTreeMap::new(),
            };
            for step in Steps::iter().flat_map(|step| step.into_iter()) {
                circuit
                    .trace
                    .insert(step, KeccakTrace::init(domain_size, step, env));
            }
            circuit
        }

        fn pad_witnesses(&mut self) {
            for opcode in Steps::iter().flat_map(|opcode| opcode.into_iter()) {
                if self.in_circuit(opcode) {
                    self.trace.get_mut(&opcode).unwrap().pad_dummy(());
                }
            }
        }
    }

    impl Tracer<N_ZKVM_KECCAK_REL_COLS, KeccakConfig, KeccakEnv<ScalarField<KeccakConfig>>>
        for KeccakTrace
    {
        type Selector = ();

        fn init(
            domain_size: usize,
            selector: Steps,
            _env: &mut KeccakEnv<ScalarField<KeccakConfig>>,
        ) -> Self {
            // Make sure we are using the same round number to refer to round steps
            let step = standardize(selector);
            Self {
                domain_size,
                witness: Witness {
                    cols: Box::new(std::array::from_fn(|_| Vec::with_capacity(domain_size))),
                },
                constraints: KeccakEnv::constraints_of(step),
                lookups: KeccakEnv::lookups_of(step),
            }
        }

        fn push_row(
            &mut self,
            _selector: Self::Selector,
            row: &[ScalarField<KeccakConfig>; N_ZKVM_KECCAK_REL_COLS],
        ) {
            for (i, value) in row.iter().enumerate() {
                if self.witness.cols[i].len() < self.witness.cols[i].capacity() {
                    self.witness.cols[i].push(*value);
                }
            }
        }

        fn pad_with_row(
            &mut self,
            _selector: Self::Selector,
            row: &[ScalarField<KeccakConfig>; N_ZKVM_KECCAK_REL_COLS],
        ) -> usize {
            let len = self.witness.cols[0].len();
            assert!(len <= self.domain_size);
            let rows_to_add = self.domain_size - len;
            // When we reach the domain size, we don't need to pad anymore.
            for _ in 0..rows_to_add {
                self.push_row((), row);
            }
            rows_to_add
        }

        fn pad_with_zeros(&mut self, _selector: Self::Selector) -> usize {
            let len = self.witness.cols[0].len();
            assert!(len <= self.domain_size);
            let rows_to_add = self.domain_size - len;
            // When we reach the domain size, we don't need to pad anymore.
            for col in self.witness.cols.iter_mut() {
                col.extend((0..rows_to_add).map(|_| ScalarField::<KeccakConfig>::zero()));
            }
            rows_to_add
        }

        fn pad_dummy(&mut self, _selector: Self::Selector) -> usize {
            // We keep track of the first row of the non-empty witness, which is a real step witness.
            let row = array::from_fn(|i| self.witness.cols[i][0]);
            self.pad_with_row(_selector, &row)
        }
    }
}

pub mod mips {
    use crate::{
        interpreters::mips::{
            column::{N_MIPS_COLS, N_MIPS_REL_COLS},
            constraints::Env,
            interpreter::{interpret_instruction, Instruction, InterpreterEnv},
        },
        legacy::{
            folding::{mips::DecomposableMIPSFoldingConfig, ScalarField},
            trace::{DecomposableTracer, DecomposedTrace, Trace, Tracer},
        },
    };
    use ark_ff::Zero;
    use kimchi_msm::witness::Witness;
    use std::{array, collections::BTreeMap};
    use strum::IntoEnumIterator;

    /// The MIPS instruction trace
    pub type MIPSTrace = Trace<N_MIPS_COLS, DecomposableMIPSFoldingConfig>;
    /// The MIPS circuit trace
    pub type DecomposedMIPSTrace = DecomposedTrace<N_MIPS_COLS, DecomposableMIPSFoldingConfig>;

    impl DecomposableTracer<Env<ScalarField<DecomposableMIPSFoldingConfig>>> for DecomposedMIPSTrace {
        fn new(
            domain_size: usize,
            env: &mut Env<ScalarField<DecomposableMIPSFoldingConfig>>,
        ) -> Self {
            let mut circuit = Self {
                domain_size,
                trace: BTreeMap::new(),
            };
            for instr in Instruction::iter().flat_map(|step| step.into_iter()) {
                circuit
                    .trace
                    .insert(instr, <MIPSTrace>::init(domain_size, instr, env));
            }
            circuit
        }

        fn pad_witnesses(&mut self) {
            for opcode in Instruction::iter().flat_map(|opcode| opcode.into_iter()) {
                self.trace.get_mut(&opcode).unwrap().pad_dummy(());
            }
        }
    }

    impl
        Tracer<
            N_MIPS_REL_COLS,
            DecomposableMIPSFoldingConfig,
            Env<ScalarField<DecomposableMIPSFoldingConfig>>,
        > for MIPSTrace
    {
        type Selector = ();

        fn init(
            domain_size: usize,
            instr: Instruction,
            env: &mut Env<ScalarField<DecomposableMIPSFoldingConfig>>,
        ) -> Self {
            interpret_instruction(env, instr);

            let trace = Self {
                domain_size,
                witness: Witness {
                    cols: Box::new(std::array::from_fn(|_| Vec::with_capacity(domain_size))),
                },
                constraints: env.get_constraints(),
                lookups: env.get_lookups(),
            };
            // Clear for the next instruction
            env.reset();
            trace
        }

        fn push_row(
            &mut self,
            _selector: Self::Selector,
            row: &[ScalarField<DecomposableMIPSFoldingConfig>; N_MIPS_REL_COLS],
        ) {
            for (i, value) in row.iter().enumerate() {
                if self.witness.cols[i].len() < self.witness.cols[i].capacity() {
                    self.witness.cols[i].push(*value);
                }
            }
        }

        fn pad_with_row(
            &mut self,
            _selector: Self::Selector,
            row: &[ScalarField<DecomposableMIPSFoldingConfig>; N_MIPS_REL_COLS],
        ) -> usize {
            let len = self.witness.cols[0].len();
            assert!(len <= self.domain_size);
            let rows_to_add = self.domain_size - len;
            // When we reach the domain size, we don't need to pad anymore.
            for _ in 0..rows_to_add {
                self.push_row(_selector, row);
            }
            rows_to_add
        }

        fn pad_with_zeros(&mut self, _selector: Self::Selector) -> usize {
            let len = self.witness.cols[0].len();
            assert!(len <= self.domain_size);
            let rows_to_add = self.domain_size - len;
            // When we reach the domain size, we don't need to pad anymore.
            for col in self.witness.cols.iter_mut() {
                col.extend(
                    (0..rows_to_add).map(|_| ScalarField::<DecomposableMIPSFoldingConfig>::zero()),
                );
            }
            rows_to_add
        }

        fn pad_dummy(&mut self, _selector: Self::Selector) -> usize {
            // We keep track of the first row of the non-empty witness, which is a real step witness.
            let row = array::from_fn(|i| self.witness.cols[i][0]);
            self.pad_with_row(_selector, &row)
        }
    }
}
