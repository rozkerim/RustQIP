extern crate num;
extern crate rayon;

use std::cmp::max;
use std::collections::{BinaryHeap, VecDeque};
use std::collections::HashMap;

use num::complex::Complex;
use rayon::prelude::*;

use crate::measurement_ops::measure;
use crate::qubits::*;
use crate::state_ops::*;
use crate::types::Precision;
use crate::utils;

pub enum StateModifierType<P: Precision> {
    UnitaryOp(QubitOp<P>),
    MeasureState(u64, Vec<u64>)
}

pub struct StateModifier<P: Precision> {
    name: String,
    modifier: StateModifierType<P>
}

impl<P: Precision> StateModifier<P> {
    pub fn new_unitary(name: String, op: QubitOp<P>) -> StateModifier<P> {
        StateModifier {
            name,
            modifier: StateModifierType::UnitaryOp(op)
        }
    }

    pub fn new_measurement(name: String, id: u64, indices: Vec<u64>) -> StateModifier<P> {
        StateModifier {
            name,
            modifier: StateModifierType::MeasureState(id, indices)
        }
    }
}

pub struct MeasuredResults<P: Precision> {
    pub results: HashMap<u64, (u64, P)>
}

impl<P: Precision> MeasuredResults<P> {
    pub fn new() -> MeasuredResults<P> {
        MeasuredResults {
            results: HashMap::new()
        }
    }
}

/// A trait which represents the state of the qubits
pub trait QuantumState<P: Precision> {
    /// Make new state with n qubits
    fn new(n: u64) -> Self;

    /// Initialize new state with initial states.
    fn new_from_initial_states(n: u64, states: &[QubitInitialState<P>]) -> Self;

    /// Function to mutate self into the state with op applied.
    fn apply_op(&mut self, op: &QubitOp<P>);

    /// Mutate self with measurement, return result as index and probability
    fn measure(&mut self, indices: &[u64]) -> (u64, P);

    /// Consume the QuantumState object and return the state as a vector of complex numbers.
    /// `natural_order` means that qubit with index 0 is the least significant index bit, otherwise
    /// it's the largest.
    fn get_state(self, natural_order: bool) -> Vec<Complex<P>>;
}

/// A basic representation of a quantum state, given by a vector of complex numbers stored
/// locally on the machine (plus an arena of equal size to work in).
pub struct LocalQuantumState<P: Precision> {
    // A bundle with the quantum state data.
    pub n: u64,
    state: Vec<Complex<P>>,
    arena: Vec<Complex<P>>,
    multithread: bool
}

pub enum InitialState<P: Precision> {
    FullState(Vec<Complex<P>>),
    Index(u64)
}

pub type QubitInitialState<P> = (Vec<u64>, InitialState<P>);

impl<P: Precision> QuantumState<P> for LocalQuantumState<P> {
    /// Build a new LocalQuantumState
    fn new(n: u64) -> LocalQuantumState<P> {
        LocalQuantumState::new_from_initial_states(n, &vec![])
    }

    /// Build a local state using a set of initial states for subsets of the qubits.
    /// These initial states are made from the qubit handles.
    fn new_from_initial_states(n: u64, states: &[QubitInitialState<P>]) -> LocalQuantumState<P> {
        let max_init_n = states.iter().map(|(indices, _)| indices).cloned().flatten().max().map(|m| m+1);

        let n = max_init_n.map(|m| max(n, m)).unwrap_or(n);

        let mut cvec: Vec<Complex<P>> = (0.. 1 << n).map(|_| Complex::<P> {
            re: P::zero(),
            im: P::zero(),
        }).collect();

        // Assume that all unrepresented indices are in the |0> state.
        let n_fullindices: u64 = states.iter().map(|(indices, state)| {
            match state {
                InitialState::FullState(_) => indices.len() as u64,
                _ => 0
            }
        }).sum();

        // Make the index template/base
        let template: u64 = states.iter().fold(0, |acc, (indices, state)| -> u64 {
            match state {
                InitialState::Index(val_indx) => sub_to_full(n, indices, val_indx.clone(), acc),
                _ => acc
            }
        });

        let init = Complex::<P> {
            re: P::one(),
            im: P::zero()
        };
        // Go through each combination of full index locations
        (0 .. 1 << n_fullindices).for_each(|i| {
            // Calculate the offset from template, and the product of fullstates.
            let (delta_index, _, val) = states.iter().fold((0u64, 0u64, init), |acc, (indices, state) | {
                if let InitialState::FullState(vals) = state {
                    let (superindex_acc, sub_index_offset, val_acc) = acc;
                    // Now we need to make additions to the superindex by adding bits based on
                    // indices, as well as return the value given by the [sub .. sub + len] bits
                    // from i.
                    let index_mask = (1 << indices.len() as u64) - 1;
                    let val_index_bits = (i >> sub_index_offset) & index_mask;
                    let val_acc = val_acc * vals[val_index_bits as usize];

                    let superindex_delta: u64 = indices.iter().enumerate().map(|(j,indx)| {
                        let bit = (val_index_bits >> j as u64) & 1u64;
                        bit << (n - 1 - indx)
                    }).sum();
                    (superindex_acc + superindex_delta, sub_index_offset + indices.len() as u64, val_acc)
                } else {
                    acc
                }
            });
            cvec[(delta_index + template) as usize] = val;
        });

        LocalQuantumState {
            n,
            state: cvec.clone(),
            arena: cvec,
            multithread: n > PARALLEL_THRESHOLD
        }
    }

    fn apply_op(&mut self, op: &QubitOp<P>) {
        apply_op(self.n, op, &self.state, &mut self.arena, 0, 0, self.multithread);
        std::mem::swap(&mut self.state, &mut self.arena);
    }

    fn measure(&mut self, indices: &[u64]) -> (u64, P) {
        let measured_result = measure(self.n, indices, &self.state, &mut self.arena, 0, 0);
        std::mem::swap(&mut self.state, &mut self.arena);
        measured_result
    }

    fn get_state(mut self, natural_order: bool) -> Vec<Complex<P>> {
        if natural_order {
            let n = self.n;
            let state = self.state;
            let f = |(i, outputloc): (usize, &mut Complex<P>)| {
                *outputloc = state[utils::flip_bits(n as usize, i as u64) as usize];
            };

            // TODO fix parallel iteration
            if self.multithread {
                self.arena.par_iter_mut().enumerate().for_each(f);
            } else {
                self.arena.iter_mut().enumerate().for_each(f);
            }
            self.arena
        } else {
            self.state
        }
    }
}

/// Apply an QubitOp to the state `s` and return the new state.
fn fold_modify_state<P: Precision, QS: QuantumState<P>>(acc: (QS, MeasuredResults<P>), modifier: &StateModifier<P>) -> (QS, MeasuredResults<P>) {
    let (mut s, mut mr) = acc;
    match &modifier.modifier {
        StateModifierType::UnitaryOp(op) => s.apply_op(op),
        StateModifierType::MeasureState(id, indices) => {
            let result = s.measure(indices);
            mr.results.insert(id.clone(), result);
        }
    }
    (s, mr)
}


/// Builds a default state of size `n`
pub fn run<P: Precision, QS: QuantumState<P>>(q: &Qubit<P>) -> (QS, MeasuredResults<P>) {
    run_with_statebuilder(q, |qs| -> QS {
        let n: u64 = qs.iter().map(|q| q.indices.len() as u64).sum();
        QS::new(n)
    })
}

pub fn run_with_init<P: Precision, QS: QuantumState<P>>(q: &Qubit<P>, states: &[QubitInitialState<P>]) -> (QS, MeasuredResults<P>){
    run_with_statebuilder(q, |qs| -> QS {
        let n: u64 = qs.iter().map(|q| q.indices.len() as u64).sum();
        QS::new_from_initial_states(n, states)
    })
}

pub fn run_with_statebuilder<P: Precision, QS: QuantumState<P>, F: FnOnce(Vec<&Qubit<P>>) -> QS>(q: &Qubit<P>, state_builder: F) -> (QS, MeasuredResults<P>) {
    let (frontier, ops) = get_opfns_and_frontier(q);
    let state = state_builder(frontier);
    ops.into_iter().fold((state, MeasuredResults::new()), fold_modify_state)
}

/// `run` the pipeline using `LocalQuantumState`.
pub fn run_local<P: Precision>(q: &Qubit<P>) -> (LocalQuantumState<P>, MeasuredResults<P>) {
    run(q)
}

/// `run_with_init` the pipeline using `LocalQuantumState`
pub fn run_local_with_init<P: Precision>(q: &Qubit<P>, states: &[QubitInitialState<P>]) -> (LocalQuantumState<P>, MeasuredResults<P>) {
    run_with_init(q, states)
}

fn get_opfns_and_frontier<P: Precision>(q: &Qubit<P>) -> (Vec<&Qubit<P>>, Vec<&StateModifier<P>>) {
    let mut heap = BinaryHeap::new();
    heap.push(q);
    let mut frontier_qubits: Vec<&Qubit<P>> = vec![];
    let mut fn_queue = VecDeque::new();
    while heap.len() > 0 {
        if let Some(q) = heap.pop() {
            match &q.parent {
                Some(parent) => {
                    match &parent {
                        Parent::Owned(parents, modifier) => {
                            if let Some(modifier) = modifier {
                                fn_queue.push_front(modifier);
                            }
                            heap.extend(parents);
                        }
                        Parent::Shared(parent) => {
                            let parent = parent.as_ref();
                            if !qubit_in_heap(parent, &heap) {
                                heap.push(parent);
                            }
                        }
                    }
                }
                None => frontier_qubits.push(q)
            }
        }
    }
    (frontier_qubits, fn_queue.into_iter().collect())
}

fn qubit_in_heap<P: Precision>(q: &Qubit<P>, heap: &BinaryHeap<&Qubit<P>>) -> bool {
    for hq in heap {
        if hq == &q {
            return true;
        }
    }
    false
}

/// Create a circuit for the circuit given by `q`. If `natural_order`, then the
/// qubit with index 0 represents the lowest bit in the index of the state (has the smallest
/// increment when flipped), otherwise it's the largest index (which is the internal state used by
/// the simulator).
pub fn make_circuit_matrix<P: Precision>(n: u64, q: &Qubit<P>, natural_order: bool) -> Vec<Vec<Complex<P>>> {
    let indices: Vec<u64> = (0 .. n).collect();
    (0 .. 1 << n).map(|i| {
        let indx = if natural_order {
            i
        } else {
            utils::flip_bits(n as usize, i as u64)
        };
        let (state, _) = run_local_with_init(&q, &[
            (indices.clone(), InitialState::Index(indx))
        ]);
        (0 .. state.state.len()).map(|i| {
            let indx = if natural_order {
                utils::flip_bits(n as usize, i as u64) as usize
            } else {
                i
            };
            state.state[indx]
        }).collect()
    }).collect()
}