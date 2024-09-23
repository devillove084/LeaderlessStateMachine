use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use petgraph::{
    algo::tarjan_scc,
    graph::{DiGraph, NodeIndex},
};
use tokio::sync::mpsc;

use super::{
    instance::{InstanceSpace, SharedInstance},
    types::InstanceStatus,
};

pub struct Executor {
    space: Arc<InstanceSpace>,
}

impl Executor {
    pub fn new(space: Arc<InstanceSpace>) -> Self {
        Self { space }
    }

    pub async fn execute(&self, mut recv: mpsc::Receiver<SharedInstance>) {
        // a inifite loop to poll instance to execute
        loop {
            let instance = recv.recv().await;
            if instance.is_none() {
                // channel has been closed, stop recving.
                break;
            }
            let mut inner = InnerExecutor::new(self.space.clone(), instance.unwrap());
            tokio::spawn(async move {
                let scc = inner.build_scc().await;
                if let Some(scc) = scc {
                    inner.execute(scc).await;
                }
            });
        }
    }
}

struct InnerExecutor {
    space: Arc<InstanceSpace>,
    start_instance: SharedInstance,
    map: Option<HashMap<SharedInstance, NodeIndex>>,
    graph: Option<DiGraph<SharedInstance, ()>>,
}

impl InnerExecutor {
    fn new(space: Arc<InstanceSpace>, start_instance: SharedInstance) -> Self {
        Self {
            space,
            start_instance,
            map: None,
            graph: None,
        }
    }

    async fn generate_scc(&self) -> Vec<Vec<NodeIndex>> {
        let g = self.graph.as_ref().unwrap();
        tarjan_scc(g)
    }

    /// Get the graph index for the instance, if the index is missing we
    /// insert the instance into graph and return the index, otherwise
    /// return the index in the map directly.
    fn get_or_insert_index(&mut self, instance: &SharedInstance) -> NodeIndex {
        let map = self.map.as_mut().unwrap();
        let g = self.graph.as_mut().unwrap();
        if !HashMap::contains_key(map, instance) {
            let index = g.add_node(instance.clone());
            map.insert(instance.clone(), index);
            index
        } else {
            *map.get(instance).unwrap()
        }
    }

    /// Tell whether we have visited the instance while building the dep graph
    fn has_visited(&self, ins: &SharedInstance) -> bool {
        let map = self.map.as_ref().unwrap();
        map.contains_key(ins)
    }

    fn add_edge(&mut self, src: NodeIndex, dst: NodeIndex) {
        let g = self.graph.as_mut().unwrap();
        g.add_edge(src, dst, ());
    }

    /// Build the scc and generate the result vec from an instance. We'll stop inserting instance to
    /// the graph in the following condition:
    /// - the instance's status is EXECUTED, which means every following step is EXECUTED.
    ///
    /// We'll also wait for one instance in the following conditions:
    /// - the instance's status is NOT COMMITTED and NOT EXECUTED.
    /// - the instance is empty.
    ///
    /// The return value is None if there's no instance to execute.
    /// The return value is Some(Vec<...>), which is the scc vec, if there are instances to execute.
    async fn build_scc(&mut self) -> Option<Vec<Vec<NodeIndex>>> {
        // the start_instance is at least in the stage of COMMITTED
        if self
            .start_instance
            .match_status(&[InstanceStatus::Executed])
            .await
        {
            return None;
        }

        let mut queue = VecDeque::new();
        queue.push_back(self.start_instance.clone());

        // init for map and graph fields
        self.map = Some(HashMap::<SharedInstance, NodeIndex>::new());
        self.graph = Some(DiGraph::<SharedInstance, ()>::new());

        loop {
            let cur = queue.pop_front();

            // if queue is empty
            if cur.is_none() {
                break;
            }
            let cur = cur.unwrap();

            // get node index
            let cur_index = self.get_or_insert_index(&cur);
            let cur_read = cur.get_instance_read().await;
            let cur_read_inner = cur_read.as_ref().unwrap();

            for (r, d) in cur_read_inner.deps.iter().enumerate() {
                if d.is_none() {
                    continue;
                }

                let r = r.into();
                let d = d.as_ref().unwrap();

                let (d_ins, notify) = self.space.get_instance_or_notify(&r, d).await;

                let d_ins = if let Some(n) = notify {
                    n.notified().await;
                    self.space.get_instance(&r, d).await
                } else {
                    d_ins
                };

                assert!(
                    d_ins.is_some(),
                    "instance should not be none after notification"
                );

                let d_ins = d_ins.unwrap();

                if d_ins.match_status(&[InstanceStatus::Committed]).await {
                    // there might be cycle
                    if !self.has_visited(&d_ins) {
                        queue.push_back(d_ins.clone());
                    }
                    let d_index = self.get_or_insert_index(&d_ins);
                    self.add_edge(cur_index, d_index);
                }
            }
        }

        Some(self.generate_scc().await)
    }

    async fn execute(&self, scc: Vec<Vec<NodeIndex>>) {
        let g = self.graph.as_ref().unwrap();
        for each_scc in scc {
            let ins_vec = each_scc.iter().map(|index| &g[*index]);

            let mut sort_vec = Vec::with_capacity(each_scc.len());
            for (id, ins) in ins_vec.enumerate() {
                let ins_read = ins.get_instance_read().await;
                let ins_read_inner = ins_read.as_ref().unwrap();
                sort_vec.push((id, (ins_read_inner.id.replica, ins_read_inner.seq)));
            }

            sort_vec.sort_by(|a, b| {
                // Compare seq
                match a.1 .1.partial_cmp(&b.1 .1) {
                    Some(std::cmp::Ordering::Greater) => std::cmp::Ordering::Greater,
                    Some(std::cmp::Ordering::Less) => std::cmp::Ordering::Less,
                    _ => std::cmp::Ordering::Equal,
                };

                // Compare replica id
                match a.1 .0.partial_cmp(&b.1 .0) {
                    Some(std::cmp::Ordering::Greater) => std::cmp::Ordering::Greater,
                    Some(std::cmp::Ordering::Less) => std::cmp::Ordering::Less,
                    _ => std::cmp::Ordering::Equal,
                }
            });

            for (id, _) in sort_vec {
                let ins = &g[each_scc[id]];
                let mut instance_write = ins.get_instance_write().await;
                let instance_write_inner = instance_write.as_mut().unwrap();

                // It may be executed by other execution tasks
                if matches!(instance_write_inner.status, InstanceStatus::Executed) {
                    for c in &instance_write_inner.cmds {
                        // FIXME: handle execute error
                        let _ = c.execute().await;
                    }
                    instance_write_inner.status = InstanceStatus::Executed;
                }
            }
        }
    }
}
