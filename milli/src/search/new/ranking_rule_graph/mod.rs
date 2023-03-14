/*! Module implementing the graph used for the graph-based ranking rules
and its related algorithms.

A ranking rule graph is built on top of the [`QueryGraph`]: the nodes stay
the same but the edges are replaced.
*/

mod build;
mod cheapest_paths;
mod edge_docids_cache;
mod empty_paths_cache;
mod path_set;

/// Implementation of the `proximity` ranking rule
mod proximity;
/// Implementation of the `typo` ranking rule
mod typo;

use std::hash::Hash;

pub use edge_docids_cache::EdgeConditionsCache;
pub use empty_paths_cache::EmptyPathsCache;
pub use proximity::ProximityGraph;
use roaring::RoaringBitmap;
pub use typo::TypoGraph;

use super::interner::{Interned, Interner};
use super::logger::SearchLogger;
use super::small_bitmap::SmallBitmap;
use super::{QueryGraph, QueryNode, SearchContext};
use crate::Result;

/// The condition that is associated with an edge in the ranking rule graph.
///
/// Some edges are unconditional, which means that traversing them does not reduce
/// the set of candidates.
///
/// Most edges, however, have a condition attached to them. For example, for the
/// proximity ranking rule, the condition could be that a word is N-close to another one.
/// When the edge is traversed, some database operations are executed to retrieve the set
/// of documents that satisfy the condition, which reduces the list of candidate document ids.
pub enum EdgeCondition<E> {
    Unconditional,
    Conditional(Interned<E>),
}

impl<E> Copy for EdgeCondition<E> {}

impl<E> Clone for EdgeCondition<E> {
    fn clone(&self) -> Self {
        match self {
            Self::Unconditional => Self::Unconditional,
            Self::Conditional(arg0) => Self::Conditional(*arg0),
        }
    }
}

/// An edge in the ranking rule graph.
///
/// It contains:
/// 1. The source and destination nodes
/// 2. The cost of traversing this edge
/// 3. The condition associated with it
#[derive(Clone)]
pub struct Edge<E> {
    pub source_node: u16,
    pub dest_node: u16,
    pub cost: u8,
    pub condition: EdgeCondition<E>,
}

/// A trait to be implemented by a marker type to build a graph-based ranking rule.
///
/// It mostly describes how to:
/// 1. Retrieve the set of edges (their cost and condition) between two nodes.
/// 2. Compute the document ids satisfying a condition
pub trait RankingRuleGraphTrait: Sized {
    /// The condition of an edge connecting two query nodes. The condition
    /// should be sufficient to compute the edge's cost and associated document ids
    /// in [`resolve_edge_condition`](RankingRuleGraphTrait::resolve_edge_condition).
    type EdgeCondition: Sized + Clone + PartialEq + Eq + Hash;

    /// Return the label of the given edge condition, to be used when visualising
    /// the ranking rule graph.
    fn label_for_edge_condition(edge: &Self::EdgeCondition) -> String;

    /// Compute the document ids associated with the given edge condition,
    /// restricted to the given universe.
    fn resolve_edge_condition<'ctx>(
        ctx: &mut SearchContext<'ctx>,
        edge_condition: &Self::EdgeCondition,
        universe: &RoaringBitmap,
    ) -> Result<RoaringBitmap>;

    /// Return the cost and condition of the edges going from the previously visited node
    /// (with [`build_step_visit_source_node`](RankingRuleGraphTrait::build_step_visit_source_node)) to `dest_node`.
    fn build_edges<'ctx>(
        ctx: &mut SearchContext<'ctx>,
        conditions_interner: &mut Interner<Self::EdgeCondition>,
        source_node: &QueryNode,
        dest_node: &QueryNode,
    ) -> Result<Vec<(u8, EdgeCondition<Self::EdgeCondition>)>>;

    fn log_state(
        graph: &RankingRuleGraph<Self>,
        paths: &[Vec<u16>],
        empty_paths_cache: &EmptyPathsCache,
        universe: &RoaringBitmap,
        distances: &[Vec<(u16, SmallBitmap)>],
        cost: u16,
        logger: &mut dyn SearchLogger<QueryGraph>,
    );
}

/// The graph used by graph-based ranking rules.
///
/// It is built on top of a [`QueryGraph`], keeping the same nodes
/// but replacing the edges.
pub struct RankingRuleGraph<G: RankingRuleGraphTrait> {
    pub query_graph: QueryGraph,
    pub edges_store: Vec<Option<Edge<G::EdgeCondition>>>,
    pub edges_of_node: Vec<SmallBitmap>,
    pub conditions_interner: Interner<G::EdgeCondition>,
}
impl<G: RankingRuleGraphTrait> Clone for RankingRuleGraph<G> {
    fn clone(&self) -> Self {
        Self {
            query_graph: self.query_graph.clone(),
            edges_store: self.edges_store.clone(),
            edges_of_node: self.edges_of_node.clone(),
            conditions_interner: self.conditions_interner.clone(),
        }
    }
}
impl<G: RankingRuleGraphTrait> RankingRuleGraph<G> {
    /// Remove the given edge from the ranking rule graph
    pub fn remove_ranking_rule_edge(&mut self, edge_index: u16) {
        let edge_opt = &mut self.edges_store[edge_index as usize];
        let Some(edge) = &edge_opt else { return };
        let (source_node, _dest_node) = (edge.source_node, edge.dest_node);
        *edge_opt = None;

        self.edges_of_node[source_node as usize].remove(edge_index);
    }
}
