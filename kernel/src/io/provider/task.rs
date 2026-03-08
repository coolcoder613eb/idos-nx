use core::sync::atomic::Ordering;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use idos_api::io::error::IoResult;
use idos_api::io::AsyncOp;
use spin::RwLock;

use super::{AsyncOpQueue, IOProvider, OpIdGenerator, UnmappedAsyncOp};
use crate::io::async_io::AsyncOpID;
use crate::io::handle::Handle;
use crate::task::id::TaskID;
use crate::task::switching::get_current_id;

/// Inner contents of the handle generated when a child task is spawned. This
/// can be used to listen for status changes in the child task, such as when it
/// exits.
pub struct TaskIOProvider {
    child_id: TaskID,
    exit_code: RwLock<Option<u32>>,

    id_gen: OpIdGenerator,
    pending_ops: RwLock<BTreeMap<AsyncOpID, UnmappedAsyncOp>>,
}

impl TaskIOProvider {
    pub fn for_task(id: TaskID) -> Self {
        Self {
            child_id: id,
            exit_code: RwLock::new(None),

            id_gen: OpIdGenerator::new(),
            pending_ops: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn matches_task(&self, id: TaskID) -> bool {
        self.child_id == id
    }

    pub fn task_exited(&self, code: u32) {
        self.exit_code.write().replace(code);
        let ids = self.pending_ops.read().keys().cloned().collect::<Vec<_>>();
        for id in ids {
            self.async_complete(id, Ok(code));
        }
    }
}

impl IOProvider for TaskIOProvider {
    fn add_op(
        &self,
        provider_index: u32,
        op: &AsyncOp,
        args: [u32; 3],
        wake_set: Option<Handle>,
    ) -> AsyncOpID {
        let id = self.id_gen.next_id();
        let unmapped =
            UnmappedAsyncOp::from_op(op, args, wake_set.map(|handle| (get_current_id(), handle)));
        self.pending_ops.write().insert(id, unmapped);

        match self.run_op(provider_index, id) {
            Some(result) => {
                self.remove_op(id);
                let return_value = self.transform_result(op.op_code, result);
                op.return_value.store(return_value, Ordering::SeqCst);
                op.signal.store(1, Ordering::SeqCst);
            }
            None => (),
        }
        id
    }

    fn get_op(&self, id: AsyncOpID) -> Option<UnmappedAsyncOp> {
        self.pending_ops.read().get(&id).cloned()
    }

    fn remove_op(&self, id: AsyncOpID) -> Option<UnmappedAsyncOp> {
        self.pending_ops.write().remove(&id)
    }

    fn read(&self, _provider_index: u32, _id: AsyncOpID, _op: UnmappedAsyncOp) -> Option<IoResult> {
        if let Some(code) = *self.exit_code.read() {
            return Some(Ok(code));
        }
        None
    }

    fn close(&self, _provider_index: u32, _id: AsyncOpID, _op: UnmappedAsyncOp) -> Option<IoResult> {
        Some(Ok(0))
    }
}
