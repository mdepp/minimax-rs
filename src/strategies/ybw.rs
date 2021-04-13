//! An implementation of iterative deeping, with each iteration executed in parallel.
//!
//! This implementation uses the Young Brothers Wait Concept, which evaluates
//! the best guess move serially first, then parallelizes all other moves
//! using rayon. This tries to reduce redundant computation at the expense of
//! more board state clones and slightly more thread synchronization.

extern crate rayon;

use super::super::interface::*;
use super::table::*;
use super::util::*;

use rayon::prelude::*;
use std::cmp::max;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use instant::{Duration, Instant};

/// Options to use for the parallel search engine.
#[derive(Clone, Copy)]
pub struct YbwOptions {
    table_byte_size: usize,
    null_window_search: bool,
    step_increment: u8,
    max_quiescence_depth: u8,
    serial_cutoff_depth: u8,
}

impl YbwOptions {
    pub fn new() -> Self {
        YbwOptions {
            table_byte_size: 32_000_000,
            null_window_search: true,
            step_increment: 1,
            max_quiescence_depth: 0,
            serial_cutoff_depth: 1,
        }
    }
}

impl Default for YbwOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl YbwOptions {
    /// Approximately how large the transposition table should be in memory.
    pub fn with_table_byte_size(mut self, size: usize) -> Self {
        self.table_byte_size = size;
        self
    }

    /// Whether to add null-window searches to try to prune branches that are
    /// probably worse than those already found. Also known as principal
    /// variation search.
    pub fn with_null_window_search(mut self, null: bool) -> Self {
        self.null_window_search = null;
        self
    }

    /// Increment the depth by two between iterations.
    pub fn with_double_step_increment(mut self) -> Self {
        self.step_increment = 2;
        self
    }

    /// Enable [quiescence
    /// search](https://en.wikipedia.org/wiki/Quiescence_search) at the leaves
    /// of the search tree.  The Game must implement `generate_noisy_moves`
    /// for the search to know when the state has become "quiet".
    pub fn with_quiescence_search_depth(mut self, depth: u8) -> Self {
        self.max_quiescence_depth = depth;
        self
    }
}

pub struct ParallelYbw<E: Evaluator> {
    max_depth: usize,
    max_time: Duration,
    timeout: Arc<AtomicBool>,
    table: ConcurrentTable<<<E as Evaluator>::G as Game>::M>,
    //move_pool: MovePool<<E::G as Game>::M>,
    prev_value: Evaluation,
    eval: E,

    opts: YbwOptions,

    // Runtime stats for the last move generated.

    // Maximum depth used to produce the move.
    actual_depth: u8,
    // Nodes explored at each depth.
    nodes_explored: Vec<u64>,
    // Nodes explored past this depth, and thus only useful for filling TT for
    // next choose_move.
    next_depth_nodes: u64,
    // For computing the average branching factor.
    total_generate_move_calls: u64,
    total_generated_moves: u64,
    table_hits: usize,
    pv: Vec<<E::G as Game>::M>,
    wall_time: Duration,
}

impl<E: Evaluator> ParallelYbw<E> {
    pub fn new(eval: E, opts: YbwOptions) -> ParallelYbw<E> {
        let table = ConcurrentTable::new(opts.table_byte_size);
        ParallelYbw {
            max_depth: 100,
            max_time: Duration::from_secs(5),
            timeout: Arc::new(AtomicBool::new(false)),
            table,
            //move_pool: MovePool::<_>::default(),
            prev_value: 0,
            opts,
            eval,
            actual_depth: 0,
            nodes_explored: Vec::new(),
            next_depth_nodes: 0,
            total_generate_move_calls: 0,
            total_generated_moves: 0,
            table_hits: 0,
            pv: Vec::new(),
            wall_time: Duration::default(),
        }
    }

    /// Set the maximum depth to search. Disables the timeout.
    /// This can be changed between moves while reusing the transposition table.
    pub fn set_max_depth(&mut self, depth: usize) {
        self.max_depth = depth;
        self.max_time = Duration::new(0, 0);
    }

    /// Set the maximum time to compute the best move. When the timeout is
    /// hit, it returns the best move found of the previous full
    /// iteration. Unlimited max depth.
    pub fn set_timeout(&mut self, max_time: Duration) {
        self.max_time = max_time;
        self.max_depth = 100;
    }

    /// Return a human-readable summary of the last move generation.
    pub fn stats(&self) -> String {
        let total_nodes_explored: u64 = self.nodes_explored.iter().sum();
        let mean_branching_factor =
            self.total_generated_moves as f64 / self.total_generate_move_calls as f64;
        let effective_branching_factor = (*self.nodes_explored.last().unwrap_or(&0) as f64)
            .powf((self.actual_depth as f64 + 1.0).recip());
        let throughput =
            (total_nodes_explored + self.next_depth_nodes) as f64 / self.wall_time.as_secs_f64();
        format!("Explored {} nodes to depth {}. MBF={:.1} EBF={:.1}\nPartial exploration of next depth hit {} nodes.\n{} transposition table hits.\n{} nodes/sec",
		total_nodes_explored, self.actual_depth, mean_branching_factor, effective_branching_factor,
		self.next_depth_nodes, self.table_hits, throughput as usize)
    }

    #[doc(hidden)]
    pub fn root_value(&self) -> Evaluation {
        unclamp_value(self.prev_value)
    }

    /// Return what the engine considered to be the best sequence of moves
    /// from both sides.
    pub fn principal_variation(&self) -> &[<E::G as Game>::M] {
        &self.pv[..]
    }

    // Negamax only among noisy moves.
    fn noisy_negamax(
        &self, s: &mut <E::G as Game>::S, depth: u8, mut alpha: Evaluation, beta: Evaluation,
    ) -> Option<Evaluation>
    where
        <E::G as Game>::M: Copy,
    {
        if self.timeout.load(Ordering::Relaxed) {
            return None;
        }
        if let Some(winner) = E::G::get_winner(s) {
            return Some(winner.evaluate());
        }
        if depth == 0 {
            return Some(self.eval.evaluate(s));
        }

        //let mut moves = self.move_pool.alloc();
        let mut moves = Vec::new();
        E::G::generate_noisy_moves(s, &mut moves);
        if moves.is_empty() {
            // Only quiet moves remain, return leaf evaluation.
            //self.move_pool.free(moves);
            return Some(self.eval.evaluate(s));
        }

        let mut best = WORST_EVAL;
        for m in moves.iter() {
            m.apply(s);
            let value = -self.noisy_negamax(s, depth - 1, -beta, -alpha)?;
            m.undo(s);
            best = max(best, value);
            alpha = max(alpha, value);
            if alpha >= beta {
                break;
            }
        }
        //self.move_pool.free(moves);
        Some(best)
    }

    // Recursively compute negamax on the game state. Returns None if it hits the timeout.
    fn negamax(
        &self, s: &mut <E::G as Game>::S, depth: u8, mut alpha: Evaluation, mut beta: Evaluation,
    ) -> Option<Evaluation>
    where
        <E::G as Game>::S: Clone + Zobrist + Send + Sync,
        <E::G as Game>::M: Copy + Eq + Send + Sync,
        E: Sync,
    {
        if self.timeout.load(Ordering::Relaxed) {
            return None;
        }

        //self.next_depth_nodes += 1;

        if depth == 0 {
            // Evaluate quiescence search on leaf nodes.
            // Will just return the node's evaluation if quiescence search is disabled.
            return self.noisy_negamax(s, self.opts.max_quiescence_depth, alpha, beta);
        }
        if let Some(winner) = E::G::get_winner(s) {
            return Some(winner.evaluate());
        }

        let alpha_orig = alpha;
        let hash = s.zobrist_hash();
        let mut good_move = None;
        if let Some(value) = self.table.check(hash, depth, &mut good_move, &mut alpha, &mut beta) {
            return Some(value);
        }

        //let mut moves = self.move_pool.alloc();
        let mut moves = Vec::new();
        E::G::generate_moves(s, &mut moves);
        //self.total_generate_move_calls += 1;
        //self.total_generated_moves += moves.len() as u64;
        if moves.is_empty() {
            //self.move_pool.free(moves);
            return Some(WORST_EVAL);
        }
        let first_move = good_move.unwrap_or(moves[0]);

        // Evaluate first move serially.
        first_move.apply(s);
        let initial_value = -self.negamax(s, depth - 1, -beta, -alpha)?;
        first_move.undo(s);
        alpha = max(alpha, initial_value);
        let (best, best_move) = if alpha >= beta {
            // Skip search
            (initial_value, first_move)
        } else if self.opts.serial_cutoff_depth >= depth {
            // Serial search
            let mut best = initial_value;
            let mut best_move = first_move;
            let mut null_window = false;
            for &m in moves.iter() {
                if m == first_move {
                    continue;
                }
                m.apply(s);
                let value = if null_window {
                    let probe = -self.negamax(s, depth - 1, -alpha - 1, -alpha)?;
                    if probe > alpha && probe < beta {
                        // Full search fallback.
                        -self.negamax(s, depth - 1, -beta, -probe)?
                    } else {
                        probe
                    }
                } else {
                    -self.negamax(s, depth - 1, -beta, -alpha)?
                };
                m.undo(s);
                if value > best {
                    best = value;
                    best_move = m;
                }
                if value > alpha {
                    alpha = value;
                    // Now that we've found a good move, assume following moves
                    // are worse, and seek to cull them without full evaluation.
                    null_window = self.opts.null_window_search;
                }
                if alpha >= beta {
                    break;
                }
            }
            (best, best_move)
        } else {
            let alpha = AtomicI32::new(alpha);
            let best_move = Mutex::new(ValueMove::new(initial_value, first_move));
            // Parallel search
            let result = moves.par_iter().with_max_len(1).try_for_each(|&m| -> Option<()> {
                // Check to see if we're cancelled by another branch.
                let initial_alpha = alpha.load(Ordering::SeqCst);
                if initial_alpha >= beta {
                    return None;
                }

                let mut state = s.clone();
                m.apply(&mut state);
                let value = if self.opts.null_window_search && initial_alpha > alpha_orig {
                    // TODO: send reference to alpha as neg_beta to children.
                    let probe =
                        -self.negamax(&mut state, depth - 1, -initial_alpha - 1, -initial_alpha)?;
                    if probe > initial_alpha && probe < beta {
                        // Check again that we're not cancelled.
                        if alpha.load(Ordering::SeqCst) >= beta {
                            return None;
                        }
                        // Full search fallback.
                        -self.negamax(&mut state, depth - 1, -beta, -probe)?
                    } else {
                        probe
                    }
                } else {
                    -self.negamax(&mut state, depth - 1, -beta, -initial_alpha)?
                };

                alpha.fetch_max(value, Ordering::SeqCst);
                let mut bests = best_move.lock().unwrap();
                bests.max(value, m);
                Some(())
            });
            if result.is_none() {
                // Check for timeout.
                if self.timeout.load(Ordering::Relaxed) {
                    return None;
                }
            }
            best_move.into_inner().unwrap().into_inner()
        };

        self.table.concurrent_update(hash, alpha_orig, beta, depth, best, best_move);
        //self.move_pool.free(moves);
        Some(clamp_value(best))
    }
}

impl<E: Evaluator> Strategy<E::G> for ParallelYbw<E>
where
    <E::G as Game>::S: Clone + Zobrist + Send + Sync,
    <E::G as Game>::M: Copy + Eq + Send + Sync,
    E: Sync,
{
    fn choose_move(&mut self, s: &<E::G as Game>::S) -> Option<<E::G as Game>::M> {
        self.table.advance_generation();
        // Reset stats.
        self.nodes_explored.clear();
        self.next_depth_nodes = 0;
        self.total_generate_move_calls = 0;
        self.total_generated_moves = 0;
        self.actual_depth = 0;
        self.table_hits = 0;
        let start_time = Instant::now();
        // Start timer if configured.
        self.timeout = if self.max_time == Duration::new(0, 0) {
            Arc::new(AtomicBool::new(false))
        } else {
            timeout_signal(self.max_time)
        };

        let root_hash = s.zobrist_hash();
        let mut s_clone = s.clone();
        let mut best_move = None;

        let mut depth = self.max_depth as u8 % self.opts.step_increment;
        while depth <= self.max_depth as u8 {
            if self.negamax(&mut s_clone, depth + 1, WORST_EVAL, BEST_EVAL).is_none() {
                // Timeout. Return the best move from the previous depth.
                break;
            }
            let entry = self.table.lookup(root_hash).unwrap();
            best_move = entry.best_move;

            self.actual_depth = max(self.actual_depth, depth);
            self.nodes_explored.push(self.next_depth_nodes);
            self.prev_value = entry.value;
            self.next_depth_nodes = 0;
            depth += self.opts.step_increment;
            self.table.populate_pv(&mut self.pv, &mut s_clone, depth + 1);
        }
        self.wall_time = start_time.elapsed();
        best_move
    }
}
